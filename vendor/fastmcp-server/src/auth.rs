//! Authentication provider hooks for MCP servers.
//!
//! Native HTTP admits credentials only from its `Authorization` header.
//! Other transport adapters retain a legacy JSON-RPC credential fallback.
//! Successful authentication is committed exactly once to the
//! request-local [`McpContext`]; credentials and identity are never persisted
//! in session state.

use std::collections::HashMap;
use std::io::Write as _;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use fastmcp_core::{
    AccessToken, AuthContext, MAX_ACCESS_TOKEN_BYTES, McpContext, McpError, McpErrorCode,
    McpResult, Sha256Digest, sha256_bounded,
};

const ACCESS_TOKEN_FIELDS: [&str; 6] = [
    "authorization",
    "Authorization",
    "auth",
    "token",
    "access_token",
    "accessToken",
];

/// HTTP query maps retain raw names. Decode form names once before matching,
/// using the same rule for wire queries and direct transport requests.
pub(crate) fn query_has_access_credentials(query: &str) -> bool {
    url::form_urlencoded::parse(query.as_bytes()).any(|(name, _)| {
        ACCESS_TOKEN_FIELDS
            .iter()
            .any(|field| field.eq_ignore_ascii_case(&name))
    })
}

const MAX_AUTH_SUBJECT_BYTES: usize = 1024;
const MAX_AUTH_SCOPES: usize = 64;
const MAX_AUTH_SCOPE_BYTES: usize = 256;
const MAX_AUTH_CLAIM_NODES: usize = 1024;
const MAX_AUTH_CLAIM_DEPTH: usize = 32;
const MAX_AUTH_CLAIM_STRING_BYTES: usize = 16 * 1024;
const MAX_AUTH_CONTEXT_BYTES: usize = 64 * 1024;
const AUTH_BYTES_GROWTH_CHUNK: usize = 4 * 1024;
const MAX_STATIC_TOKEN_ENTRIES: usize = 4_096;
const MAX_ALLOWED_AUTH_SCHEMES: usize = 16;
const AUTHENTICATED_PRINCIPAL_DOMAIN: &[u8] = b"fastmcp/session-principal/authenticated/v1\0";
const ANONYMOUS_PRINCIPAL_DOMAIN: &[u8] = b"fastmcp/session-principal/anonymous/v1\0";

struct BoundedAuthBytes {
    bytes: Vec<u8>,
}

impl BoundedAuthBytes {
    fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    fn ensure_capacity_for(&mut self, next_size: usize) -> std::io::Result<()> {
        if next_size <= self.bytes.capacity() {
            return Ok(());
        }

        let current_capacity = self.bytes.capacity();
        let geometric_target = if current_capacity == 0 {
            AUTH_BYTES_GROWTH_CHUNK
        } else {
            current_capacity
                .checked_mul(2)
                .unwrap_or(MAX_AUTH_CONTEXT_BYTES)
                .min(MAX_AUTH_CONTEXT_BYTES)
        };
        let target_capacity = next_size.max(geometric_target).min(MAX_AUTH_CONTEXT_BYTES);

        // Allocate separately so failure leaves the previous canonical bytes
        // intact and allocator over-allocation cannot silently exceed the
        // admission limit.
        let mut grown = Vec::new();
        grown
            .try_reserve_exact(target_capacity)
            .map_err(|_| std::io::Error::other("authentication context allocation failed"))?;
        if grown.capacity() > MAX_AUTH_CONTEXT_BYTES {
            return Err(std::io::Error::other(
                "authentication context allocation exceeds byte limit",
            ));
        }
        grown.extend_from_slice(&self.bytes);
        self.bytes = grown;
        Ok(())
    }
}

impl std::io::Write for BoundedAuthBytes {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let new_len = self
            .bytes
            .len()
            .checked_add(buffer.len())
            .filter(|length| *length <= MAX_AUTH_CONTEXT_BYTES)
            .ok_or_else(|| std::io::Error::other("authentication context exceeds byte limit"))?;
        self.ensure_capacity_for(new_len)?;
        self.bytes.extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn claims_shape_is_bounded(root: &serde_json::Value) -> bool {
    let mut stack = Vec::new();
    if stack.try_reserve_exact(1).is_err() {
        return false;
    }
    stack.push((root, 0_usize));
    let mut nodes = 0_usize;

    while let Some((value, depth)) = stack.pop() {
        nodes = match nodes.checked_add(1) {
            Some(nodes) if nodes <= MAX_AUTH_CLAIM_NODES => nodes,
            _ => return false,
        };
        if depth > MAX_AUTH_CLAIM_DEPTH {
            return false;
        }
        match value {
            serde_json::Value::String(value) => {
                if value.len() > MAX_AUTH_CLAIM_STRING_BYTES {
                    return false;
                }
            }
            serde_json::Value::Array(values) => {
                for value in values {
                    if nodes.saturating_add(stack.len()) >= MAX_AUTH_CLAIM_NODES
                        || stack.try_reserve(1).is_err()
                    {
                        return false;
                    }
                    stack.push((value, depth.saturating_add(1)));
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    if key.len() > MAX_AUTH_CLAIM_STRING_BYTES
                        || nodes.saturating_add(stack.len()) >= MAX_AUTH_CLAIM_NODES
                        || stack.try_reserve(1).is_err()
                    {
                        return false;
                    }
                    stack.push((value, depth.saturating_add(1)));
                }
            }
            serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => {
            }
        }
    }
    true
}

pub(crate) fn principal_fingerprint(auth: Option<&AuthContext>) -> McpResult<Sha256Digest> {
    let mut canonical = BoundedAuthBytes::new();
    match auth {
        None => canonical
            .write_all(ANONYMOUS_PRINCIPAL_DOMAIN)
            .map_err(|_| McpError::internal_error("authentication admission exceeds bounds"))?,
        Some(auth) => {
            if auth
                .subject
                .as_ref()
                .is_some_and(|subject| subject.is_empty() || subject.len() > MAX_AUTH_SUBJECT_BYTES)
                || auth.scopes.len() > MAX_AUTH_SCOPES
                || auth
                    .scopes
                    .iter()
                    .any(|scope| scope.is_empty() || scope.len() > MAX_AUTH_SCOPE_BYTES)
                || auth
                    .claims
                    .as_ref()
                    .is_some_and(|claims| !claims_shape_is_bounded(claims))
            {
                return Err(McpError::internal_error(
                    "authentication provider returned facts outside admission bounds",
                ));
            }
            let mut admitted_facts = BoundedAuthBytes::new();
            serde_json::to_writer(&mut admitted_facts, auth).map_err(|_| {
                McpError::internal_error(
                    "authentication provider returned facts outside admission bounds",
                )
            })?;
            canonical
                .write_all(AUTHENTICATED_PRINCIPAL_DOMAIN)
                .map_err(|_| McpError::internal_error("authentication admission exceeds bounds"))?;
            match (auth.session_owner(), auth.subject.as_deref()) {
                (Some(owner), _) => {
                    canonical.write_all(&[2]).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                    canonical.write_all(owner.as_bytes()).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                }
                (None, None) if auth.scopes.is_empty() && auth.claims.is_none() => {
                    canonical.write_all(&[0]).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                }
                (None, None) => {
                    return Err(McpError::internal_error(
                        "authentication provider returned ownerless authorization facts",
                    ));
                }
                (None, Some(subject)) => {
                    canonical.write_all(&[1]).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                    let length = u64::try_from(subject.len()).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                    canonical.write_all(&length.to_be_bytes()).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                    canonical.write_all(subject.as_bytes()).map_err(|_| {
                        McpError::internal_error("authentication admission exceeds bounds")
                    })?;
                }
            }
        }
    }
    sha256_bounded(&canonical.bytes, MAX_AUTH_CONTEXT_BYTES)
        .map_err(|_| McpError::internal_error("authentication admission exceeds bounds"))
}

/// Authentication request view used by providers.
#[derive(Clone, Copy)]
pub struct AuthRequest<'a> {
    /// JSON-RPC method name.
    pub method: &'a str,
    /// Raw params payload (if present).
    ///
    /// Inspecting credentials in JSON-RPC params is a legacy,
    /// transport-neutral fallback. Transport integrations should authenticate
    /// from their native authorization metadata instead. Any credential field
    /// recognized here is removed before extension middleware and handlers
    /// receive the request.
    pub params: Option<&'a serde_json::Value>,
    /// Transport-private `Authorization` field, when the transport has one.
    ///
    /// This value is never inserted into JSON-RPC params or exposed to
    /// middleware and handlers. When present it must satisfy the strict native
    /// header grammar, and no legacy in-band credential may coexist with it.
    pub transport_authorization: Option<&'a str>,
    /// Internal request ID (u64) used for tracing.
    pub request_id: u64,
}

impl std::fmt::Debug for AuthRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRequest")
            .field("method_bytes", &self.method.len())
            .field("params_present", &self.params.is_some())
            .field(
                "transport_authorization_present",
                &self.transport_authorization.is_some(),
            )
            .field("request_id", &self.request_id)
            .finish()
    }
}

impl AuthRequest<'_> {
    /// Returns the one strictly admitted native or legacy access token.
    ///
    /// Malformed or ambiguous credential sources return `None`; the server
    /// rejects those cases before invoking an authentication provider.
    #[must_use]
    pub fn access_token(&self) -> Option<AccessToken> {
        admitted_access_token(*self).ok().flatten()
    }

    /// Returns true when more than one recognized credential source is
    /// present, including multiple legacy in-band locations.
    #[must_use]
    pub fn has_multiple_credential_sources(&self) -> bool {
        matches!(
            admitted_access_token(*self),
            Err(CredentialSourceError::Multiple)
        )
    }

    pub(crate) fn credential_sources_are_admissible(&self) -> bool {
        admitted_access_token(*self).is_ok()
    }

    pub(crate) fn has_any_credential_source(&self) -> bool {
        self.transport_authorization.is_some() || self.has_in_band_credential_source()
    }

    pub(crate) fn has_in_band_credential_source(&self) -> bool {
        single_in_band_credential(self.params).is_some()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CredentialSourceError {
    Malformed,
    Multiple,
}

fn admitted_access_token(
    request: AuthRequest<'_>,
) -> Result<Option<AccessToken>, CredentialSourceError> {
    let in_band = single_in_band_credential(request.params);
    match request.transport_authorization {
        Some(_) if in_band.is_some() => Err(CredentialSourceError::Multiple),
        Some(authorization) => parse_native_authorization(authorization)
            .map(Some)
            .ok_or(CredentialSourceError::Malformed),
        None => match in_band.transpose()? {
            Some(value) => extract_from_value(value).map(Some),
            None => Ok(None),
        },
    }
}

fn record_credential_candidate<'a>(
    candidate: &mut Option<&'a serde_json::Value>,
    value: &'a serde_json::Value,
) -> Result<(), CredentialSourceError> {
    if candidate.replace(value).is_some() {
        return Err(CredentialSourceError::Multiple);
    }
    Ok(())
}

fn scan_credential_map<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    candidate: &mut Option<&'a serde_json::Value>,
) -> Result<(), CredentialSourceError> {
    for key in ACCESS_TOKEN_FIELDS {
        if let Some(value) = map.get(key) {
            record_credential_candidate(candidate, value)?;
        }
    }
    Ok(())
}

fn is_header_access_token_field(key: &str) -> bool {
    ACCESS_TOKEN_FIELDS
        .iter()
        .any(|field| field.eq_ignore_ascii_case(key))
}

fn scan_headers_credential_map<'a>(
    map: &'a serde_json::Map<String, serde_json::Value>,
    candidate: &mut Option<&'a serde_json::Value>,
) -> Result<(), CredentialSourceError> {
    for (key, value) in map {
        if is_header_access_token_field(key) {
            record_credential_candidate(candidate, value)?;
        }
    }
    Ok(())
}

fn single_in_band_credential(
    params: Option<&serde_json::Value>,
) -> Option<Result<&serde_json::Value, CredentialSourceError>> {
    let params = params?;
    if matches!(params, serde_json::Value::String(_)) {
        return Some(Ok(params));
    }
    let serde_json::Value::Object(map) = params else {
        return None;
    };

    let mut candidate = None;
    if let Err(error) = scan_credential_map(map, &mut candidate) {
        return Some(Err(error));
    }
    if let Some(nested) = map.get("_meta").and_then(serde_json::Value::as_object)
        && let Err(error) = scan_credential_map(nested, &mut candidate)
    {
        return Some(Err(error));
    }
    if let Some(nested_meta_headers) = map
        .get("_meta")
        .and_then(serde_json::Value::as_object)
        .and_then(|meta| meta.get("headers"))
        .and_then(serde_json::Value::as_object)
        && let Err(error) = scan_headers_credential_map(nested_meta_headers, &mut candidate)
    {
        return Some(Err(error));
    }
    if let Some(nested_headers) = map.get("headers").and_then(serde_json::Value::as_object)
        && let Err(error) = scan_headers_credential_map(nested_headers, &mut candidate)
    {
        return Some(Err(error));
    }
    candidate.map(Ok)
}

fn parse_native_authorization(value: &str) -> Option<AccessToken> {
    AccessToken::parse(value)
}

fn extract_from_value(value: &serde_json::Value) -> Result<AccessToken, CredentialSourceError> {
    match value {
        serde_json::Value::String(value) => {
            AccessToken::parse_legacy_in_band(value).ok_or(CredentialSourceError::Malformed)
        }
        serde_json::Value::Object(map) => {
            let scheme = map.get("scheme");
            let token = map.get("token");
            let alternative_keys = [
                "authorization",
                "Authorization",
                "access_token",
                "accessToken",
            ];
            let alternative_count = alternative_keys
                .iter()
                .filter(|key| map.contains_key(**key))
                .count();

            if scheme.is_some() || token.is_some() {
                if alternative_count != 0 {
                    return Err(CredentialSourceError::Multiple);
                }
                let scheme = scheme
                    .and_then(serde_json::Value::as_str)
                    .ok_or(CredentialSourceError::Malformed)?;
                let token = token
                    .and_then(serde_json::Value::as_str)
                    .ok_or(CredentialSourceError::Malformed)?;
                let access = AccessToken::from_parts(scheme, token)
                    .ok_or(CredentialSourceError::Malformed)?;
                return (access.scheme == scheme && access.token == token)
                    .then_some(access)
                    .ok_or(CredentialSourceError::Malformed);
            }

            if alternative_count > 1 {
                return Err(CredentialSourceError::Multiple);
            }
            if alternative_count == 0 {
                return Err(CredentialSourceError::Malformed);
            }
            let value = alternative_keys
                .iter()
                .find_map(|key| map.get(*key))
                .and_then(serde_json::Value::as_str)
                .ok_or(CredentialSourceError::Malformed)?;
            AccessToken::parse_legacy_in_band(value).ok_or(CredentialSourceError::Malformed)
        }
        _ => Err(CredentialSourceError::Malformed),
    }
}

/// Removes only the JSON locations treated as in-band credentials by
/// [`AuthRequest::access_token`].
///
/// This intentionally does not recurse into tool arguments or arbitrary
/// application objects. Fields that authentication recognizes must not remain
/// visible to extension middleware or handlers after the provider has used
/// them, while unrelated protocol and application data is preserved.
pub(crate) fn strip_recognized_access_credentials(params: &mut Option<serde_json::Value>) {
    // A bare string is supported only as a legacy token-only payload, so
    // there is no non-credential parameter value to retain.
    if matches!(params, Some(serde_json::Value::String(_))) {
        *params = None;
        return;
    }

    let Some(serde_json::Value::Object(map)) = params.as_mut() else {
        return;
    };
    remove_access_token_fields(map);
    if let Some(nested) = map
        .get_mut("_meta")
        .and_then(serde_json::Value::as_object_mut)
    {
        remove_access_token_fields(nested);
        if let Some(nested_headers) = nested
            .get_mut("headers")
            .and_then(serde_json::Value::as_object_mut)
        {
            remove_headers_access_token_fields(nested_headers);
        }
    }
    if let Some(nested_headers) = map
        .get_mut("headers")
        .and_then(serde_json::Value::as_object_mut)
    {
        remove_headers_access_token_fields(nested_headers);
    }
}

fn remove_access_token_fields(map: &mut serde_json::Map<String, serde_json::Value>) {
    for key in ACCESS_TOKEN_FIELDS {
        map.remove(key);
    }
}

fn remove_headers_access_token_fields(map: &mut serde_json::Map<String, serde_json::Value>) {
    map.retain(|key, _| !is_header_access_token_field(key));
}

/// Authentication provider interface.
///
/// Implementations decide whether a request is allowed and may return
/// an [`AuthContext`] describing the authenticated subject.
pub trait AuthProvider: Send + Sync {
    /// Authenticate an incoming request.
    ///
    /// Return `Ok(AuthContext)` to allow, or an `Err(McpError)` to deny. When
    /// admitting a credential, `AuthContext::subject` must be a nonempty,
    /// stable, provider-scoped owner identifier. Scopes and claims are
    /// authorization facts and are deliberately not session-owner identity.
    /// Provider error messages and data are treated as private diagnostics and
    /// are replaced at the framework boundary before middleware or peers see
    /// them.
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext>;
}

/// Token verifier interface used by token-based auth providers.
pub trait TokenVerifier: Send + Sync {
    /// Verify an access token and return an auth context if valid.
    fn verify(
        &self,
        ctx: &McpContext,
        request: AuthRequest<'_>,
        token: &AccessToken,
    ) -> McpResult<AuthContext>;
}

/// Token-based authentication provider.
#[derive(Clone)]
pub struct TokenAuthProvider {
    verifier: Arc<dyn TokenVerifier>,
    missing_token_error: McpError,
}

impl TokenAuthProvider {
    /// Creates a new token auth provider with the given verifier.
    #[must_use]
    pub fn new<V: TokenVerifier + 'static>(verifier: V) -> Self {
        Self {
            verifier: Arc::new(verifier),
            missing_token_error: auth_error("Missing access token"),
        }
    }

    /// Overrides the error returned when a token is missing.
    #[must_use]
    pub fn with_missing_token_error(mut self, error: McpError) -> Self {
        self.missing_token_error = error;
        self
    }
}

impl AuthProvider for TokenAuthProvider {
    fn authenticate(&self, ctx: &McpContext, request: AuthRequest<'_>) -> McpResult<AuthContext> {
        let access = match admitted_access_token(request) {
            Ok(Some(token)) => token,
            Ok(None) => return Err(self.missing_token_error.clone()),
            Err(CredentialSourceError::Multiple) => {
                return Err(auth_error("Multiple conflicting credential sources"));
            }
            Err(CredentialSourceError::Malformed) => {
                return Err(auth_error("Invalid access token"));
            }
        };
        self.verifier.verify(ctx, request, &access)
    }
}

/// Static-token authentication with a shared, live-updatable digest registry.
///
/// Clone this verifier before installing it in [`TokenAuthProvider`] to retain
/// an administrative handle. All clones observe token replacement, rotation,
/// expiry, and revocation without rebuilding the provider or restarting a
/// listener. Configure allowed schemes before cloning; that policy belongs to
/// each handle and is not changed by registry updates.
///
/// Mutations are atomic with respect to subsequent verification decisions.
/// They do not cancel already-admitted work or close connections whose
/// transport authenticates only at connection establishment. This is an
/// in-memory registry, not a persistent revocation store or an OAuth issuer.
/// Raw credentials are never retained: only bounded fixed-width digests and
/// admitted authentication facts enter the registry.
#[derive(Clone)]
pub struct StaticTokenVerifier {
    // TokenVerifier is synchronous. These short in-memory critical sections
    // contain no I/O, await, or application callbacks; configuration admission
    // and disposal of replaced maps happen outside the lock.
    tokens: Arc<RwLock<HashMap<Sha256Digest, StaticTokenEntry>>>,
    allowed_schemes: Option<Vec<String>>,
}

struct StaticTokenEntry {
    context: AuthContext,
    expires_at: Option<Instant>,
}

impl std::fmt::Debug for StaticTokenVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StaticTokenVerifier")
            .field(
                "token_count",
                &self.tokens.try_read().ok().map(|tokens| tokens.len()),
            )
            .field(
                "allowed_scheme_count",
                &self.allowed_schemes.as_ref().map_or(0, Vec::len),
            )
            .finish()
    }
}

impl StaticTokenVerifier {
    /// Creates a new static verifier from a token → context map.
    ///
    /// Every configured credential must resolve to an admissible authenticated
    /// owner. Rejecting ownerless or oversized facts here prevents a verifier
    /// configuration that succeeds in isolation but is guaranteed to fail at
    /// the server's authentication-admission boundary.
    pub fn new<I, K>(tokens: I) -> McpResult<Self>
    where
        I: IntoIterator<Item = (K, AuthContext)>,
        K: Into<String>,
    {
        Self::new_with_expiry(
            tokens
                .into_iter()
                .map(|(token, context)| (token, context, None)),
        )
    }

    /// Creates a verifier with credentials and their deadlines admitted
    /// together, before any credential can be verified.
    ///
    /// Each entry is `(token, context, deadline)`. `None` means no expiration;
    /// a deadline at or before the verification instant denies access. Expired
    /// entries are allowed in the configuration but never authenticate. This
    /// constructor does not interpret wall-clock claims such as JWT `exp`.
    pub fn new_with_expiry<I, K>(tokens: I) -> McpResult<Self>
    where
        I: IntoIterator<Item = (K, AuthContext, Option<Instant>)>,
        K: Into<String>,
    {
        Ok(Self {
            tokens: Arc::new(RwLock::new(Self::admit_tokens(tokens)?)),
            allowed_schemes: None,
        })
    }

    fn admit_tokens<I, K>(tokens: I) -> McpResult<HashMap<Sha256Digest, StaticTokenEntry>>
    where
        I: IntoIterator<Item = (K, AuthContext, Option<Instant>)>,
        K: Into<String>,
    {
        let mut digests = HashMap::new();
        for (token, context, expires_at) in tokens {
            if digests.len() >= MAX_STATIC_TOKEN_ENTRIES {
                return Err(auth_error("Static token configuration is invalid"));
            }
            if context.subject.as_deref().is_none_or(str::is_empty)
                || principal_fingerprint(Some(&context)).is_err()
            {
                return Err(auth_error("Static token configuration is invalid"));
            }
            let token = token.into();
            if !AccessToken::is_valid_token68(&token) {
                return Err(auth_error("Static token configuration is invalid"));
            }
            let digest = sha256_bounded(token.as_bytes(), MAX_ACCESS_TOKEN_BYTES)
                .map_err(|_| auth_error("Static token configuration is invalid"))?;
            digests
                .try_reserve(1)
                .map_err(|_| auth_error("Static token configuration is invalid"))?;
            if digests
                .insert(
                    digest,
                    StaticTokenEntry {
                        context,
                        expires_at,
                    },
                )
                .is_some()
            {
                return Err(auth_error("Static token configuration is invalid"));
            }
        }
        if digests.is_empty() {
            return Err(auth_error("Static token configuration is invalid"));
        }
        Ok(digests)
    }

    /// Atomically replaces the complete credential set for every clone.
    ///
    /// Invalid or empty replacement input leaves the previous policy intact;
    /// use [`Self::revoke_all`] for deliberate deny-all. Existing deadlines
    /// follow retained credentials, while new credentials have no expiration.
    /// Use [`Self::replace_tokens_with_expiry`] to install new credentials and
    /// their deadlines atomically, without an intervening unbounded grant.
    pub fn replace_tokens<I, K>(&self, tokens: I) -> McpResult<()>
    where
        I: IntoIterator<Item = (K, AuthContext)>,
        K: Into<String>,
    {
        self.replace_tokens_with_expiry(
            tokens
                .into_iter()
                .map(|(token, context)| (token, context, None)),
        )
    }

    /// Atomically replaces credentials together with optional expiry deadlines.
    ///
    /// Validation, duplicate detection, and allocation of the bounded new map
    /// finish before the active map is changed. For a retained credential, the
    /// earlier of its existing and proposed deadlines wins: a reload cannot
    /// extend, remove, or revive an already-expired entry's deadline. Newly
    /// introduced entries use the supplied deadline, including immediate
    /// expiry, from their first verification. This is an administrative grant
    /// operation: explicitly adding a previously removed credential grants it
    /// again according to the newly supplied policy.
    pub fn replace_tokens_with_expiry<I, K>(&self, tokens: I) -> McpResult<()>
    where
        I: IntoIterator<Item = (K, AuthContext, Option<Instant>)>,
        K: Into<String>,
    {
        let mut replacement = Self::admit_tokens(tokens)?;
        let mut active = self
            .tokens
            .write()
            .map_err(|_| auth_error("Static token registry is unavailable"))?;
        for (digest, entry) in &mut replacement {
            if let Some(previous) = active.get(digest) {
                entry.expires_at = match (previous.expires_at, entry.expires_at) {
                    (Some(old), Some(new)) => Some(old.min(new)),
                    (Some(old), None) => Some(old),
                    (None, proposed) => proposed,
                };
            }
        }
        let retired = std::mem::replace(&mut *active, replacement);
        drop(active);
        drop(retired);
        Ok(())
    }

    /// Revokes exactly one credential across every installed clone.
    ///
    /// Returns whether the credential was present. Repeated revocation is
    /// harmless. Malformed input or a poisoned registry fails without changing
    /// the credential set, and errors never include the credential.
    pub fn revoke_token(&self, token: &str) -> McpResult<bool> {
        let digest = Self::token_digest(token)?;
        let mut active = self
            .tokens
            .write()
            .map_err(|_| auth_error("Static token registry is unavailable"))?;
        let removed = active.remove(&digest);
        drop(active);
        Ok(removed.is_some())
    }

    /// Revokes all credentials atomically and returns the number removed.
    ///
    /// Every subsequent verification fails until an administrator explicitly
    /// installs a new set using [`Self::replace_tokens`].
    pub fn revoke_all(&self) -> McpResult<usize> {
        let mut active = self
            .tokens
            .write()
            .map_err(|_| auth_error("Static token registry is unavailable"))?;
        let retired = std::mem::take(&mut *active);
        drop(active);
        Ok(retired.len())
    }

    /// Sets or shortens a credential's monotonic expiration deadline.
    ///
    /// The credential is refused at and after `deadline`. A past deadline
    /// expires it immediately. Repeated calls may shorten, but never extend,
    /// an existing deadline; rotation and replacement preserve that deadline.
    /// Returns `false` for an unknown credential without adding an entry.
    /// No wall-clock claims are interpreted implicitly.
    pub fn expire_token_at(&self, token: &str, deadline: Instant) -> McpResult<bool> {
        let digest = Self::token_digest(token)?;
        let mut active = self
            .tokens
            .write()
            .map_err(|_| auth_error("Static token registry is unavailable"))?;
        let Some(entry) = active.get_mut(&digest) else {
            return Ok(false);
        };
        entry.expires_at = Some(entry.expires_at.map_or(deadline, |old| old.min(deadline)));
        Ok(true)
    }

    /// Atomically replaces one live secret while preserving its identity,
    /// scopes, claims, and expiration deadline.
    ///
    /// The old secret must still be present and unexpired; the new secret must
    /// be different and not already registered. Any refusal leaves the active
    /// credentials unchanged. Rotation works at the entry limit because it
    /// does not increase the credential count. Neither secret is returned or
    /// included in an error message.
    pub fn rotate_token(&self, old_token: &str, new_token: &str) -> McpResult<()> {
        let old_digest = Self::token_digest(old_token)?;
        let new_digest = Self::token_digest(new_token)?;
        if old_digest == new_digest {
            return Err(auth_error("Static token rotation is invalid"));
        }
        let mut active = self
            .tokens
            .write()
            .map_err(|_| auth_error("Static token registry is unavailable"))?;
        if active.contains_key(&new_digest) || !active.contains_key(&old_digest) {
            return Err(auth_error("Static token rotation is invalid"));
        }
        // Reserve before removing the old entry, so allocation failure cannot
        // accidentally revoke it. No fallible operation follows removal.
        active
            .try_reserve(1)
            .map_err(|_| auth_error("Static token rotation is invalid"))?;
        let now = Instant::now();
        if active
            .get(&old_digest)
            .is_some_and(|entry| entry.expires_at.is_some_and(|deadline| now >= deadline))
        {
            return Err(auth_error("Static token rotation is invalid"));
        }
        let Some(entry) = active.remove(&old_digest) else {
            return Err(auth_error("Static token rotation is invalid"));
        };
        active.insert(new_digest, entry);
        Ok(())
    }

    fn token_digest(token: &str) -> McpResult<Sha256Digest> {
        if !AccessToken::is_valid_token68(token) {
            return Err(auth_error("Invalid access token"));
        }
        sha256_bounded(token.as_bytes(), MAX_ACCESS_TOKEN_BYTES)
            .map_err(|_| auth_error("Invalid access token"))
    }

    /// Restricts accepted token schemes (case-insensitive).
    pub fn with_allowed_schemes<I, S>(mut self, schemes: I) -> McpResult<Self>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut admitted = Vec::new();
        for scheme in schemes {
            if admitted.len() >= MAX_ALLOWED_AUTH_SCHEMES {
                return Err(auth_error("Static auth scheme configuration is invalid"));
            }
            let scheme = scheme.into();
            if !AccessToken::is_valid_http_scheme(&scheme) {
                return Err(auth_error("Static auth scheme configuration is invalid"));
            }
            let normalized = scheme.to_ascii_lowercase();
            if admitted.iter().any(|existing| existing == &normalized) {
                return Err(auth_error("Static auth scheme configuration is invalid"));
            }
            admitted
                .try_reserve(1)
                .map_err(|_| auth_error("Static auth scheme configuration is invalid"))?;
            admitted.push(normalized);
        }
        if admitted.is_empty() {
            return Err(auth_error("Static auth scheme configuration is invalid"));
        }
        self.allowed_schemes = Some(admitted);
        Ok(self)
    }

    fn verify_with_clock(
        &self,
        token: &AccessToken,
        clock: impl FnOnce() -> Instant,
    ) -> McpResult<AuthContext> {
        if !AccessToken::is_valid_http_scheme(&token.scheme)
            || !AccessToken::is_valid_token68(&token.token)
        {
            return Err(auth_error("Invalid access token"));
        }
        if let Some(allowed) = &self.allowed_schemes {
            let normalized = token.scheme.to_ascii_lowercase();
            if !allowed.contains(&normalized) {
                return Err(auth_error("Unsupported auth scheme"));
            }
        }

        let digest = Self::token_digest(&token.token)?;
        let active = self
            .tokens
            .read()
            .map_err(|_| auth_error("Invalid access token"))?;
        // Sample after acquiring the registry lock, not before a possibly
        // contended acquisition. Tests drive this same path at exact deadlines.
        let now = clock();
        let Some(entry) = active.get(&digest) else {
            return Err(auth_error("Invalid access token"));
        };
        if entry.expires_at.is_some_and(|deadline| now >= deadline) {
            return Err(auth_error("Invalid access token"));
        }
        Ok(entry.context.clone())
    }
}

impl TokenVerifier for StaticTokenVerifier {
    fn verify(
        &self,
        _ctx: &McpContext,
        _request: AuthRequest<'_>,
        token: &AccessToken,
    ) -> McpResult<AuthContext> {
        self.verify_with_clock(token, Instant::now)
    }
}

fn auth_error(message: impl Into<String>) -> McpError {
    McpError::new(McpErrorCode::ResourceForbidden, message)
}

/// Default allow-all provider (returns anonymous auth context).
#[derive(Debug, Default, Clone, Copy)]
pub struct AllowAllAuthProvider;

impl AuthProvider for AllowAllAuthProvider {
    fn authenticate(&self, _ctx: &McpContext, _request: AuthRequest<'_>) -> McpResult<AuthContext> {
        Ok(AuthContext::anonymous())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;

    fn ctx() -> McpContext {
        McpContext::new(Cx::for_testing(), 1)
    }

    #[test]
    fn access_token_parsers_separate_http_and_legacy_in_band_grammar() {
        assert_eq!(
            fastmcp_core::AccessToken::parse("Bearer abc"),
            Some(fastmcp_core::AccessToken {
                scheme: "Bearer".to_string(),
                token: "abc".to_string(),
            })
        );
        assert_eq!(
            fastmcp_core::AccessToken::parse_legacy_in_band("abc"),
            Some(fastmcp_core::AccessToken {
                scheme: "Bearer".to_string(),
                token: "abc".to_string(),
            })
        );
        // Bare token parsing treats the entire value as a bearer token, even if it
        // happens to be the literal string "Bearer".
        assert_eq!(
            fastmcp_core::AccessToken::parse_legacy_in_band(" Bearer"),
            Some(fastmcp_core::AccessToken {
                scheme: "Bearer".to_string(),
                token: "Bearer".to_string(),
            })
        );
        assert_eq!(fastmcp_core::AccessToken::parse(""), None);
        assert_eq!(fastmcp_core::AccessToken::parse("   "), None);
        assert_eq!(fastmcp_core::AccessToken::parse("Bearer "), None);
        assert_eq!(fastmcp_core::AccessToken::parse("abc"), None);
        assert_eq!(fastmcp_core::AccessToken::parse("Bearer abc:def"), None);
    }

    #[test]
    fn auth_request_extracts_access_token_from_common_locations() {
        // params as string
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&serde_json::Value::String("Bearer t1".to_string())),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(
            req.access_token(),
            Some(AccessToken {
                scheme: "Bearer".to_string(),
                token: "t1".to_string(),
            })
        );

        // params as object with authorization field
        let params = serde_json::json!({"authorization": "Bearer t2"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(
            req.access_token(),
            Some(AccessToken {
                scheme: "Bearer".to_string(),
                token: "t2".to_string(),
            })
        );

        // params as object with {scheme, token}
        let params = serde_json::json!({"auth": {"scheme": "Bearer", "token": "t3"}});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(
            req.access_token(),
            Some(AccessToken {
                scheme: "Bearer".to_string(),
                token: "t3".to_string(),
            })
        );

        // params as object with _meta.authorization
        let params = serde_json::json!({"_meta": {"authorization": "Bearer t4"}});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(
            req.access_token(),
            Some(AccessToken {
                scheme: "Bearer".to_string(),
                token: "t4".to_string(),
            })
        );

        // params as object with headers.Authorization
        let params = serde_json::json!({"headers": {"Authorization": "Bearer t5"}});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(
            req.access_token(),
            Some(AccessToken {
                scheme: "Bearer".to_string(),
                token: "t5".to_string(),
            })
        );
    }

    #[test]
    fn native_and_in_band_authorization_are_rejected_as_ambiguous() {
        let params = serde_json::json!({"authorization": "Bearer in-band"});
        let request = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Token native"),
            request_id: 1,
        };

        assert!(request.access_token().is_none());
        assert!(request.has_multiple_credential_sources());
    }

    #[test]
    fn malformed_native_authorization_cannot_fall_back_in_band() {
        let params = serde_json::json!({"authorization": "Bearer in-band"});
        let request = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer "),
            request_id: 1,
        };

        assert!(request.access_token().is_none());
    }

    #[test]
    fn native_authorization_requires_http_token_scheme_and_token68_credential() {
        for malformed in [
            "Bearer",
            "Basic",
            "Bearer one two",
            "Bearer\tcredential",
            "Be(arer token",
            "Bearer ab=c",
            "Bearer credential,other",
            " Bearer credential",
            "Bearer credential ",
        ] {
            let request = AuthRequest {
                method: "tools/call",
                params: None,
                transport_authorization: Some(malformed),
                request_id: 1,
            };
            assert!(
                request.access_token().is_none(),
                "accepted malformed native authorization: {malformed:?}"
            );
            assert!(!request.credential_sources_are_admissible());
        }

        let request = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: Some("Bearer abc_DEF-123+/=="),
            request_id: 1,
        };
        let token = request.access_token().expect("valid native authorization");
        assert_eq!(token.scheme, "Bearer");
        assert_eq!(token.token, "abc_DEF-123+/==");
    }

    #[test]
    fn normalized_http_authorization_still_uses_strict_native_grammar() {
        use fastmcp_transport::http::HttpTransport;
        use std::io::Cursor;

        for (wire_value, accepted) in [
            ("Bearer    ", false),
            ("Basic", false),
            ("Bearer\tcredential", false),
            ("Bearer one two", false),
            ("Bearer abc_DEF-123==", true),
        ] {
            let wire = format!(
                "POST /mcp HTTP/1.1\r\nHost: localhost\r\nAuthorization: {wire_value}\r\nContent-Length: 0\r\n\r\n"
            );
            let mut transport = HttpTransport::new(Cursor::new(wire.into_bytes()), Vec::new());
            let http_request = transport.read_request().expect("parse HTTP request");
            let auth_request = AuthRequest {
                method: "tools/list",
                params: None,
                transport_authorization: http_request.authorization(),
                request_id: 1,
            };
            assert_eq!(
                auth_request.access_token().is_some(),
                accepted,
                "unexpected admission for normalized Authorization {wire_value:?}"
            );
        }
    }

    #[test]
    fn auth_request_detects_multiple_credential_sources() {
        let params = serde_json::json!({"_meta": {"accessToken": "Bearer in-band"}});
        let multiple = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer native"),
            request_id: 1,
        };
        assert!(multiple.has_multiple_credential_sources());

        let application_params = serde_json::json!({"arguments": {"token": "application-data"}});
        let native_only = AuthRequest {
            method: "tools/call",
            params: Some(&application_params),
            transport_authorization: Some("Bearer native"),
            request_id: 2,
        };
        assert!(!native_only.has_multiple_credential_sources());
    }

    #[test]
    fn principal_fingerprint_separates_anonymous_from_authenticated_facts() {
        let anonymous = principal_fingerprint(None).expect("anonymous fingerprint");
        let admitted_anonymous =
            principal_fingerprint(Some(&AuthContext::anonymous())).expect("provider fingerprint");
        let alice =
            principal_fingerprint(Some(&AuthContext::with_subject("alice"))).expect("fingerprint");
        let alice_again =
            principal_fingerprint(Some(&AuthContext::with_subject("alice"))).expect("fingerprint");

        assert_ne!(anonymous, admitted_anonymous);
        assert_ne!(anonymous, alice);
        assert_ne!(admitted_anonymous, alice);
        assert_eq!(alice, alice_again);

        let mut alice_with_changed_authorization = AuthContext::with_subject("alice");
        alice_with_changed_authorization.scopes = vec!["write".to_string(), "read".to_string()];
        alice_with_changed_authorization.claims =
            Some(serde_json::json!({"exp": 99, "policy_revision": 2}));
        assert_eq!(
            alice,
            principal_fingerprint(Some(&alice_with_changed_authorization))
                .expect("stable owner fingerprint")
        );
        assert_ne!(
            alice,
            principal_fingerprint(Some(&AuthContext::with_subject("bob")))
                .expect("different owner fingerprint")
        );
    }

    #[test]
    fn principal_fingerprint_rejects_out_of_bounds_provider_facts() {
        let oversized_subject = AuthContext::with_subject("s".repeat(MAX_AUTH_SUBJECT_BYTES + 1));
        assert!(principal_fingerprint(Some(&oversized_subject)).is_err());

        assert!(principal_fingerprint(Some(&AuthContext::with_subject(""))).is_err());

        let mut empty_scope = AuthContext::anonymous();
        empty_scope.scopes = vec![String::new()];
        assert!(principal_fingerprint(Some(&empty_scope)).is_err());

        let mut too_many_scopes = AuthContext::anonymous();
        too_many_scopes.scopes = vec!["scope".to_string(); MAX_AUTH_SCOPES + 1];
        assert!(principal_fingerprint(Some(&too_many_scopes)).is_err());

        let mut oversized_claim = AuthContext::anonymous();
        oversized_claim.claims = Some(serde_json::Value::String(
            "c".repeat(MAX_AUTH_CLAIM_STRING_BYTES + 1),
        ));
        assert!(principal_fingerprint(Some(&oversized_claim)).is_err());

        let mut oversized_aggregate = AuthContext::with_subject("owner");
        oversized_aggregate.claims = Some(serde_json::Value::Array(vec![
            serde_json::Value::String(
                "c".repeat(MAX_AUTH_CLAIM_STRING_BYTES)
            );
            5
        ]));
        assert!(claims_shape_is_bounded(
            oversized_aggregate.claims.as_ref().unwrap()
        ));
        assert!(principal_fingerprint(Some(&oversized_aggregate)).is_err());
    }

    #[test]
    fn token_auth_provider_errors_on_missing_token_and_allows_override() {
        #[derive(Debug)]
        struct AcceptAll;
        impl TokenVerifier for AcceptAll {
            fn verify(
                &self,
                _ctx: &McpContext,
                _request: AuthRequest<'_>,
                _token: &AccessToken,
            ) -> McpResult<AuthContext> {
                Ok(AuthContext::with_subject("ok"))
            }
        }

        let provider = TokenAuthProvider::new(AcceptAll);
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let err = provider.authenticate(&ctx(), req).unwrap_err();
        assert_eq!(err.code, McpErrorCode::ResourceForbidden);
        assert!(err.message.contains("Missing access token"));

        let provider =
            TokenAuthProvider::new(AcceptAll).with_missing_token_error(auth_error("no token"));
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let err = provider.authenticate(&ctx(), req).unwrap_err();
        assert!(err.message.contains("no token"));
    }

    #[test]
    fn static_token_verifier_enforces_scheme_without_exposing_token() {
        let mut base = AuthContext::with_subject("user123");
        base.scopes = vec!["read".to_string()];

        let verifier = StaticTokenVerifier::new([("value-1", base.clone())])
            .expect("valid verifier configuration")
            .with_allowed_schemes(["Bearer"])
            .expect("valid scheme configuration");
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };

        // Wrong scheme
        let err = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "Basic".to_string(),
                    token: "value-1".to_string(),
                },
            )
            .unwrap_err();
        assert!(err.message.contains("Unsupported auth scheme"));

        // Valid scheme (case-insensitive)
        let auth = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "bearer".to_string(),
                    token: "value-1".to_string(),
                },
            )
            .unwrap();
        assert_eq!(auth.subject, Some("user123".to_string()));
        assert_eq!(auth.scopes, vec!["read".to_string()]);
        let serialized = serde_json::to_string(&auth).expect("serialize verified auth facts");
        assert!(!serialized.contains("value-1"));
    }

    #[test]
    fn allow_all_provider_returns_anonymous_context() {
        let provider = AllowAllAuthProvider;
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let auth = provider.authenticate(&ctx(), req).unwrap();
        assert_eq!(auth.subject, None);
        assert_eq!(auth.scopes.len(), 0);
    }

    #[test]
    fn access_token_from_none_params() {
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_array_params() {
        let params = serde_json::json!([1, 2, 3]);
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_number_params() {
        let params = serde_json::json!(42);
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_object_with_token_field() {
        let params = serde_json::json!({"token": "Bearer my-secret"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should extract token");
        assert_eq!(token.scheme, "Bearer");
        assert_eq!(token.token, "my-secret");
    }

    #[test]
    fn access_token_from_object_with_access_token_field() {
        let params = serde_json::json!({"access_token": "abc123"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should extract");
        // Bare token defaults to Bearer scheme
        assert_eq!(token.scheme, "Bearer");
        assert_eq!(token.token, "abc123");
    }

    #[test]
    fn access_token_from_camel_case_field() {
        let params = serde_json::json!({"accessToken": "Bearer xyz"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should extract");
        assert_eq!(token.token, "xyz");
    }

    #[test]
    fn access_token_from_nested_scheme_token_object_rejects_empty_scheme() {
        let params = serde_json::json!({"auth": {"scheme": "", "token": "abc"}});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert_eq!(req.access_token(), None);
    }

    #[test]
    fn access_token_from_nested_scheme_token_object_with_whitespace_token() {
        // Whitespace-only token should be rejected by the scheme/token path
        // and also by the legacy in-band parser, which trims empty values.
        let params = serde_json::json!({"authorization": "  "});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn static_verifier_rejects_unknown_token() {
        let verifier =
            StaticTokenVerifier::new([("valid-token", AuthContext::with_subject("owner"))])
                .expect("valid verifier configuration");
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let err = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: "wrong-token".to_string(),
                },
            )
            .unwrap_err();
        assert_eq!(err.code, McpErrorCode::ResourceForbidden);
        assert!(err.message.contains("Invalid access token"));
    }

    #[test]
    fn static_verifier_no_scheme_restriction_allows_any() {
        let verifier = StaticTokenVerifier::new([("tok", AuthContext::with_subject("alice"))])
            .expect("valid verifier configuration");
        let req = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let auth = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "CustomScheme".to_string(),
                    token: "tok".to_string(),
                },
            )
            .unwrap();
        assert_eq!(auth.subject, Some("alice".to_string()));
    }

    #[test]
    fn token_auth_provider_succeeds_with_valid_token() {
        let verifier = StaticTokenVerifier::new([("secret", AuthContext::with_subject("bob"))])
            .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);
        let params = serde_json::json!({"authorization": "Bearer secret"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let auth = provider.authenticate(&ctx(), req).unwrap();
        assert_eq!(auth.subject, Some("bob".to_string()));
    }

    #[test]
    fn token_auth_provider_fails_with_wrong_token() {
        let verifier = StaticTokenVerifier::new([("secret", AuthContext::with_subject("bob"))])
            .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);
        let params = serde_json::json!({"authorization": "Bearer wrong"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let err = provider.authenticate(&ctx(), req).unwrap_err();
        assert_eq!(err.code, McpErrorCode::ResourceForbidden);
    }

    #[test]
    fn auth_request_debug() {
        let params = serde_json::json!({"AUTH_PARAMS_DEBUG_CANARY": "AUTH_VALUE_DEBUG_CANARY"});
        let req = AuthRequest {
            method: "AUTH_METHOD_DEBUG_CANARY",
            params: Some(&params),
            transport_authorization: None,
            request_id: 42,
        };
        let debug = format!("{req:?}");
        assert!(debug.contains("method_bytes"));
        assert!(debug.contains("42"));
        assert!(!debug.contains("AUTH_METHOD_DEBUG_CANARY"));
        assert!(!debug.contains("AUTH_PARAMS_DEBUG_CANARY"));
        assert!(!debug.contains("AUTH_VALUE_DEBUG_CANARY"));
    }

    #[test]
    fn credential_stripping_removes_only_recognized_locations() {
        let mut params = Some(serde_json::json!({
            "authorization": "Bearer top-secret",
            "auth": {"token": "nested-top-secret"},
            "_meta": {
                "accessToken": "meta-secret",
                "trace": "keep-meta"
            },
            "headers": {
                "Authorization": "Bearer header-secret",
                "content-type": "application/json"
            },
            "arguments": {
                "token": "application-data",
                "nested": {"authorization": "application-data-too"}
            },
            "name": "tool-name"
        }));

        strip_recognized_access_credentials(&mut params);

        assert_eq!(
            params,
            Some(serde_json::json!({
                "_meta": {"trace": "keep-meta"},
                "headers": {"content-type": "application/json"},
                "arguments": {
                    "token": "application-data",
                    "nested": {"authorization": "application-data-too"}
                },
                "name": "tool-name"
            }))
        );
    }

    #[test]
    fn credential_stripping_removes_legacy_bare_string_payload() {
        let mut params = Some(serde_json::json!("Bearer secret"));
        strip_recognized_access_credentials(&mut params);
        assert_eq!(params, None);
    }

    #[test]
    fn auth_request_clone_copy() {
        let req = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let req2 = req; // Copy
        assert_eq!(req.method, req2.method);
        assert_eq!(req.request_id, req2.request_id);
    }

    #[test]
    fn access_token_from_headers_nested_object() {
        // headers containing an object with scheme and token
        let params = serde_json::json!({
            "headers": {
                "Authorization": {"scheme": "Bearer", "token": "hdr-tok"}
            }
        });
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should extract from headers");
        assert_eq!(token.scheme, "Bearer");
        assert_eq!(token.token, "hdr-tok");
    }

    #[test]
    fn access_token_from_empty_object() {
        let params = serde_json::json!({});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    // ── AllowAllAuthProvider derives ─────────────────────────────────

    #[test]
    fn allow_all_provider_debug() {
        let provider = AllowAllAuthProvider;
        let debug = format!("{provider:?}");
        assert!(debug.contains("AllowAllAuthProvider"));
    }

    #[test]
    fn allow_all_provider_default() {
        let _ = AllowAllAuthProvider;
    }

    #[test]
    fn allow_all_provider_clone_copy() {
        let provider = AllowAllAuthProvider;
        let cloned = provider.clone();
        let copied = provider; // Copy
        let _ = cloned
            .authenticate(
                &ctx(),
                AuthRequest {
                    method: "test",
                    params: None,
                    transport_authorization: None,
                    request_id: 1,
                },
            )
            .unwrap();
        let _ = copied;
    }

    // ── TokenAuthProvider ────────────────────────────────────────────

    #[test]
    fn token_auth_provider_clone() {
        let verifier =
            StaticTokenVerifier::new([("tok", AuthContext::with_subject("clone-owner"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);
        let cloned = provider.clone();
        let params = serde_json::json!({"authorization": "Bearer tok"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let auth = cloned.authenticate(&ctx(), req).unwrap();
        assert_eq!(auth.subject.as_deref(), Some("clone-owner"));
    }

    #[test]
    fn token_auth_provider_with_custom_error_and_valid_token() {
        let verifier = StaticTokenVerifier::new([("valid", AuthContext::with_subject("user"))])
            .expect("valid verifier configuration");
        let provider =
            TokenAuthProvider::new(verifier).with_missing_token_error(auth_error("custom missing"));
        let params = serde_json::json!({"authorization": "Bearer valid"});
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let auth = provider.authenticate(&ctx(), req).unwrap();
        assert_eq!(auth.subject, Some("user".to_string()));
    }

    // ── StaticTokenVerifier ──────────────────────────────────────────

    #[test]
    fn static_verifier_debug_redacts_configured_tokens() {
        let canary = "STATIC_TOKEN_DEBUG_CANARY";
        let verifier = StaticTokenVerifier::new([(canary, AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration");
        let debug = format!("{verifier:?}");
        assert!(debug.contains("StaticTokenVerifier"));
        assert!(debug.contains("token_count"));
        assert!(!debug.contains(canary));
    }

    #[test]
    fn static_verifier_fails_closed_for_oversized_configured_or_presented_tokens() {
        let oversized = "x".repeat(MAX_ACCESS_TOKEN_BYTES + 1);
        let configured = StaticTokenVerifier::new([(
            oversized.clone(),
            AuthContext::with_subject("must-not-load"),
        )]);
        let config_error = configured.expect_err("oversized configured token must fail closed");
        assert_eq!(config_error.code, McpErrorCode::ResourceForbidden);
        assert!(!config_error.message.contains(&oversized));

        let verifier = StaticTokenVerifier::new([("valid", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration");
        let request = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let error = verifier
            .verify(
                &ctx(),
                request,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: oversized.clone(),
                },
            )
            .expect_err("oversized presented token must fail closed");
        assert_eq!(error.code, McpErrorCode::ResourceForbidden);
        assert!(!error.message.contains(&oversized));
    }

    #[test]
    fn static_verifier_multiple_tokens() {
        let verifier = StaticTokenVerifier::new([
            ("alpha", AuthContext::with_subject("alice")),
            ("beta", AuthContext::with_subject("bob")),
        ])
        .expect("valid verifier configuration");
        let req = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let a = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: "alpha".to_string(),
                },
            )
            .unwrap();
        assert_eq!(a.subject, Some("alice".to_string()));
        let b = verifier
            .verify(
                &ctx(),
                req,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: "beta".to_string(),
                },
            )
            .unwrap();
        assert_eq!(b.subject, Some("bob".to_string()));
    }

    #[test]
    fn static_verifier_multiple_allowed_schemes() {
        let verifier = StaticTokenVerifier::new([("tok", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration")
            .with_allowed_schemes(["Bearer", "Token"])
            .expect("valid scheme configuration");
        let req = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        // Bearer works
        assert!(
            verifier
                .verify(
                    &ctx(),
                    req,
                    &AccessToken {
                        scheme: "Bearer".to_string(),
                        token: "tok".to_string(),
                    },
                )
                .is_ok()
        );
        // Token works
        assert!(
            verifier
                .verify(
                    &ctx(),
                    req,
                    &AccessToken {
                        scheme: "Token".to_string(),
                        token: "tok".to_string(),
                    },
                )
                .is_ok()
        );
        // Basic does not
        assert!(
            verifier
                .verify(
                    &ctx(),
                    req,
                    &AccessToken {
                        scheme: "Basic".to_string(),
                        token: "tok".to_string(),
                    },
                )
                .is_err()
        );
    }

    // ── extract_from_value edge cases ────────────────────────────────

    #[test]
    fn access_token_from_bool_value_returns_none() {
        let params = serde_json::json!(true);
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_null_params() {
        let params = serde_json::json!(null);
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_nested_object_with_inner_authorization_string() {
        // Object with auth field pointing to object that has an inner authorization string
        let params = serde_json::json!({
            "auth": {
                "authorization": "Bearer inner-tok"
            }
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should extract from nested auth");
        assert_eq!(token.token, "inner-tok");
    }

    // ── _meta and headers fallback priority ──────────────────────────

    #[test]
    fn access_token_meta_fallback_when_top_level_empty() {
        let params = serde_json::json!({
            "other_field": 123,
            "_meta": {"authorization": "Bearer meta-tok"}
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should fallback to _meta");
        assert_eq!(token.token, "meta-tok");
    }

    #[test]
    fn access_token_headers_fallback_when_top_and_meta_empty() {
        let params = serde_json::json!({
            "other": "value",
            "_meta": {"other": "value"},
            "headers": {"authorization": "Bearer hdr-tok"}
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should fallback to headers");
        assert_eq!(token.token, "hdr-tok");
    }

    #[test]
    fn access_token_rejects_multiple_in_band_locations() {
        let params = serde_json::json!({
            "authorization": "Bearer top-tok",
            "_meta": {"authorization": "Bearer meta-tok"}
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
        assert!(req.has_multiple_credential_sources());
    }

    #[test]
    fn access_token_reports_multiple_sources_inside_nested_credential_object() {
        for params in [
            serde_json::json!({
                "auth": {
                    "authorization": "Bearer first",
                    "access_token": "Bearer second"
                }
            }),
            serde_json::json!({
                "auth": {
                    "scheme": "Bearer",
                    "token": "first",
                    "authorization": "Bearer second"
                }
            }),
        ] {
            let req = AuthRequest {
                method: "test",
                params: Some(&params),
                transport_authorization: None,
                request_id: 1,
            };
            assert!(req.access_token().is_none());
            assert!(req.has_multiple_credential_sources());
            assert!(!req.credential_sources_are_admissible());
        }
    }

    // ── auth_error helper ────────────────────────────────────────────

    #[test]
    fn auth_error_creates_resource_forbidden() {
        let err = auth_error("denied");
        assert_eq!(err.code, McpErrorCode::ResourceForbidden);
        assert!(err.message.contains("denied"));
    }

    // ── extract_from_value with non-matching object ──────────────────

    #[test]
    fn access_token_from_object_without_any_known_key() {
        let params = serde_json::json!({"unknown_key": "Bearer tok"});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_from_scheme_token_with_whitespace_only_scheme() {
        let params = serde_json::json!({"auth": {"scheme": "  ", "token": "abc"}});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
        assert!(!req.credential_sources_are_admissible());
    }

    // ── _meta / headers non-object fallthrough ──────────────────────

    #[test]
    fn access_token_meta_non_object_falls_through_to_headers() {
        let params = serde_json::json!({
            "_meta": 42,
            "headers": {"authorization": "Bearer hdr"}
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req.access_token().expect("should skip non-object _meta");
        assert_eq!(token.token, "hdr");
    }

    #[test]
    fn access_token_headers_non_object_returns_none() {
        let params = serde_json::json!({
            "_meta": {"other": true},
            "headers": "not-an-object"
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    // ── Non-string, non-object values in map fields ─────────────────

    #[test]
    fn access_token_map_field_with_numeric_value_returns_none() {
        let params = serde_json::json!({"authorization": 12345});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_map_field_with_bool_value_returns_none() {
        let params = serde_json::json!({"token": true});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    #[test]
    fn access_token_map_field_with_array_value_returns_none() {
        let params = serde_json::json!({"authorization": ["Bearer", "tok"]});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        assert!(req.access_token().is_none());
    }

    // ── extract_from_value nested accessToken key ───────────────────

    #[test]
    fn access_token_nested_object_with_access_token_key() {
        let params = serde_json::json!({
            "auth": {
                "accessToken": "Bearer nested-at"
            }
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let token = req
            .access_token()
            .expect("should extract from nested accessToken");
        assert_eq!(token.token, "nested-at");
    }

    // ── StaticTokenVerifier with empty allowed_schemes ──────────────

    #[test]
    fn static_verifier_rejects_empty_allowed_scheme_configuration() {
        let error = StaticTokenVerifier::new([("tok", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration")
            .with_allowed_schemes(Vec::<String>::new())
            .expect_err("an empty scheme policy is a configuration error");
        assert_eq!(error.code, McpErrorCode::ResourceForbidden);
    }

    #[test]
    fn static_verifier_rejects_malformed_and_duplicate_configuration() {
        for context in [AuthContext::anonymous(), AuthContext::with_subject("")] {
            let error = StaticTokenVerifier::new([("token", context)])
                .expect_err("a configured credential must have a nonempty owner subject");
            assert_eq!(error.code, McpErrorCode::ResourceForbidden);
            assert_eq!(error.message, "Static token configuration is invalid");
        }

        let oversized_subject = AuthContext::with_subject("x".repeat(MAX_AUTH_SUBJECT_BYTES + 1));
        let error = StaticTokenVerifier::new([("token", oversized_subject)])
            .expect_err("inadmissible authentication facts must fail at configuration time");
        assert_eq!(error.code, McpErrorCode::ResourceForbidden);
        assert_eq!(error.message, "Static token configuration is invalid");

        for token in ["", " ", " leading", "trailing ", "two words", "bad:token"] {
            assert!(
                StaticTokenVerifier::new([(token, AuthContext::with_subject("owner"))]).is_err(),
                "accepted malformed static token {token:?}"
            );
        }

        assert!(
            StaticTokenVerifier::new([
                ("duplicate", AuthContext::with_subject("first")),
                ("duplicate", AuthContext::with_subject("second")),
            ])
            .is_err()
        );

        let verifier = StaticTokenVerifier::new([("token", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration");
        assert!(verifier.with_allowed_schemes(["Bearer", "bearer"]).is_err());
        let verifier = StaticTokenVerifier::new([("token", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration");
        assert!(verifier.with_allowed_schemes(["Bad Scheme"]).is_err());
    }

    #[test]
    fn static_verifier_enforces_exact_entry_and_scheme_boundaries() {
        let empty = StaticTokenVerifier::new(Vec::<(String, AuthContext)>::new())
            .expect_err("an empty static-token map is not an authentication policy");
        assert_eq!(empty.code, McpErrorCode::ResourceForbidden);

        let maximum_entries = (0..MAX_STATIC_TOKEN_ENTRIES)
            .map(|index| {
                (
                    format!("token-{index}"),
                    AuthContext::with_subject(format!("owner-{index}")),
                )
            })
            .collect::<Vec<_>>();
        assert_eq!(
            StaticTokenVerifier::new(maximum_entries)
                .expect("the documented entry maximum is admissible")
                .tokens
                .read()
                .expect("registry lock")
                .len(),
            MAX_STATIC_TOKEN_ENTRIES
        );

        let excessive_entries = (0..=MAX_STATIC_TOKEN_ENTRIES)
            .map(|index| {
                (
                    format!("token-{index}"),
                    AuthContext::with_subject(format!("owner-{index}")),
                )
            })
            .collect::<Vec<_>>();
        assert!(StaticTokenVerifier::new(excessive_entries).is_err());

        let maximum_schemes = (0..MAX_ALLOWED_AUTH_SCHEMES)
            .map(|index| format!("Scheme{index}"))
            .collect::<Vec<_>>();
        StaticTokenVerifier::new([("token", AuthContext::with_subject("owner"))])
            .expect("valid verifier configuration")
            .with_allowed_schemes(maximum_schemes)
            .expect("the documented scheme maximum is admissible");

        let excessive_schemes = (0..=MAX_ALLOWED_AUTH_SCHEMES)
            .map(|index| format!("Scheme{index}"))
            .collect::<Vec<_>>();
        assert!(
            StaticTokenVerifier::new([("token", AuthContext::with_subject("owner"))])
                .expect("valid verifier configuration")
                .with_allowed_schemes(excessive_schemes)
                .is_err()
        );
    }

    // ── TokenAuthProvider with scheme restriction in verifier ────────

    #[test]
    fn token_auth_provider_with_scheme_restriction() {
        let verifier = StaticTokenVerifier::new([("secret", AuthContext::with_subject("user"))])
            .expect("valid verifier configuration")
            .with_allowed_schemes(["Bearer"])
            .expect("valid scheme configuration");
        let provider = TokenAuthProvider::new(verifier);

        // Basic scheme rejected by verifier
        let params = serde_json::json!({"authorization": "Basic secret"});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let err = provider.authenticate(&ctx(), req).unwrap_err();
        assert!(err.message.contains("Unsupported"));

        // Bearer scheme accepted
        let params = serde_json::json!({"authorization": "Bearer secret"});
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };
        let auth = provider.authenticate(&ctx(), req).unwrap();
        assert_eq!(auth.subject, Some("user".to_string()));
    }

    // ── AuthRequest with all fields populated ───────────────────────

    #[test]
    fn auth_request_exposes_all_fields() {
        let params = serde_json::json!({"key": "val"});
        let req = AuthRequest {
            method: "prompts/get",
            params: Some(&params),
            transport_authorization: None,
            request_id: 99,
        };
        assert_eq!(req.method, "prompts/get");
        assert_eq!(req.request_id, 99);
        assert!(req.params.is_some());
    }

    // ── AUTH-01 Implementation A unit coverage ──────────────────────
    //
    // These exercise credential-source admission against crate internals.
    // They deliberately do NOT carry the frozen AUTH-01 A acceptance IDs:
    // a `cfg(test)` module cannot prove shipped behavior (PL-3), and the
    // frozen IDs must resolve to exactly one location workspace-wide (PL-1).
    // The frozen `auth_01_a_positive` / `auth_01_a_planted_negative` pair
    // lives in the external public-surface consumer at
    // `crates/fastmcp-server/tests/auth_01_a.rs`.

    #[test]
    fn auth_01_a_unit_positive() {
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);
        let params = serde_json::json!({
            "headers": {
                "content-type": "application/json"
            },
            "arguments": {
                "Token": "ordinary-arg"
            }
        });
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer valid-token-123"),
            request_id: 1,
        };

        assert!(req.credential_sources_are_admissible());
        assert!(!req.has_multiple_credential_sources());
        let token = req.access_token().expect("native transport token admitted");
        assert_eq!(token.scheme, "Bearer");
        assert_eq!(token.token, "valid-token-123");

        let auth = provider
            .authenticate(&ctx(), req)
            .expect("provider authenticates");
        assert_eq!(auth.subject.as_deref(), Some("alice"));
        let fingerprint = principal_fingerprint(Some(&auth)).expect("fingerprint computed");
        assert_eq!(
            fingerprint,
            principal_fingerprint(Some(&AuthContext::with_subject("alice"))).unwrap()
        );
    }

    #[test]
    fn auth_01_a_unit_planted_negative() {
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(verifier);
        // Near-identical request differing ONLY in the forbidden dimension:
        // params.headers introduces a nested legacy authorization header.
        let params = serde_json::json!({
            "headers": {
                "content-type": "application/json",
                "AUTHORIZATION": "Bearer in-band-token"
            },
            "arguments": {
                "Token": "ordinary-arg"
            }
        });
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer valid-token-123"),
            request_id: 1,
        };

        assert!(!req.credential_sources_are_admissible());
        assert!(req.has_multiple_credential_sources());
        assert!(req.access_token().is_none());

        let err = provider
            .authenticate(&ctx(), req)
            .expect_err("must deny ambiguous credential");
        assert_eq!(err.code, McpErrorCode::ResourceForbidden);

        // Prove stripping removes the in-band authorization header while preserving
        // non-credential headers and tool arguments.
        let mut stripped_params = Some(params);
        strip_recognized_access_credentials(&mut stripped_params);
        assert_eq!(
            stripped_params,
            Some(serde_json::json!({
                "headers": {
                    "content-type": "application/json"
                },
                "arguments": {
                    "Token": "ordinary-arg"
                }
            }))
        );
    }

    struct CountingVerifier {
        inner: StaticTokenVerifier,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl TokenVerifier for CountingVerifier {
        fn verify(
            &self,
            ctx: &McpContext,
            request: AuthRequest<'_>,
            token: &AccessToken,
        ) -> McpResult<AuthContext> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.inner.verify(ctx, request, token)
        }
    }

    #[test]
    fn auth_01_a_nested_meta_headers_legacy_positive() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        });
        let legacy_params = serde_json::json!({
            "_meta": {
                "headers": {
                    "Authorization": "Bearer valid-token-123",
                    "content-type": "application/json"
                }
            },
            "arguments": {"Token": "ordinary-arg"}
        });
        let legacy_request = AuthRequest {
            method: "tools/call",
            params: Some(&legacy_params),
            transport_authorization: None,
            request_id: 1,
        };

        let legacy_auth = provider
            .authenticate(&ctx(), legacy_request)
            .expect("legacy nested header credential is admitted");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(legacy_auth.subject.as_deref(), Some("alice"));

        let native_params = serde_json::json!({
            "_meta": {
                "headers": {"content-type": "application/json"}
            },
            "arguments": {"Token": "ordinary-arg"}
        });
        let native_request = AuthRequest {
            method: "tools/call",
            params: Some(&native_params),
            transport_authorization: Some("Bearer valid-token-123"),
            request_id: 1,
        };
        let native_auth = provider
            .authenticate(&ctx(), native_request)
            .expect("native Authorization credential is admitted");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
        assert_eq!(
            principal_fingerprint(Some(&legacy_auth)).expect("legacy fingerprint"),
            principal_fingerprint(Some(&native_auth)).expect("native fingerprint")
        );

        let mut stripped = Some(legacy_params);
        strip_recognized_access_credentials(&mut stripped);
        assert_eq!(
            stripped,
            Some(serde_json::json!({
                "_meta": {
                    "headers": {"content-type": "application/json"}
                },
                "arguments": {"Token": "ordinary-arg"}
            }))
        );
    }

    #[test]
    fn auth_01_a_nested_meta_headers_native_planted_negative() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        });
        let params = serde_json::json!({
            "_meta": {
                "headers": {
                    "Authorization": "Bearer valid-token-123",
                    "content-type": "application/json"
                }
            },
            "arguments": {"Token": "ordinary-arg"}
        });
        let original = params.clone();
        let request = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer valid-token-123"),
            request_id: 1,
        };

        assert!(!request.credential_sources_are_admissible());
        assert!(request.has_multiple_credential_sources());
        let error = provider
            .authenticate(&ctx(), request)
            .expect_err("native and nested legacy credentials must be rejected");
        assert_eq!(error.code, McpErrorCode::ResourceForbidden);
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(params, original);

        let mut stripped = Some(params);
        strip_recognized_access_credentials(&mut stripped);
        assert_eq!(
            stripped,
            Some(serde_json::json!({
                "_meta": {
                    "headers": {"content-type": "application/json"}
                },
                "arguments": {"Token": "ordinary-arg"}
            }))
        );
    }

    #[test]
    fn nested_headers_mixed_case_duplicate_with_canonical_spelling_rejected() {
        let params = serde_json::json!({
            "headers": {
                "authorization": "Bearer canon",
                "AUTHORIZATION": "Bearer uppercase"
            }
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };

        assert!(req.access_token().is_none());
        assert!(req.has_multiple_credential_sources());
        assert!(!req.credential_sources_are_admissible());
    }

    #[test]
    fn nested_headers_native_plus_mixed_case_source_rejection() {
        for header_key in [
            "authorization",
            "Authorization",
            "AUTHORIZATION",
            "AuThOrIzAtIoN",
        ] {
            let params = serde_json::json!({
                "headers": {
                    header_key: "Bearer in-band"
                }
            });
            let req = AuthRequest {
                method: "tools/call",
                params: Some(&params),
                transport_authorization: Some("Bearer native"),
                request_id: 1,
            };
            assert!(
                req.access_token().is_none(),
                "should reject conflicting source for header spelling {header_key}"
            );
            assert!(req.has_multiple_credential_sources());
            assert!(!req.credential_sources_are_admissible());
        }
    }

    #[test]
    fn nested_headers_stripping_removes_all_recognized_header_spellings_and_aliases() {
        let mut params = Some(serde_json::json!({
            "headers": {
                "AUTHORIZATION": "Bearer secret1",
                "AuThOrIzAtIoN": "Bearer secret2",
                "token": "token-secret",
                "content-type": "application/json",
                "x-request-id": "12345"
            },
            "arguments": {
                "Token": "application-argument"
            }
        }));

        strip_recognized_access_credentials(&mut params);

        assert_eq!(
            params,
            Some(serde_json::json!({
                "headers": {
                    "content-type": "application/json",
                    "x-request-id": "12345"
                },
                "arguments": {
                    "Token": "application-argument"
                }
            }))
        );
    }

    #[test]
    fn auth_01_a_header_map_alias_case_insensitive_positive() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        });

        for (request_id, header_key) in [
            (1, "AUTH"),
            (2, "TOKEN"),
            (3, "ACCESS_TOKEN"),
            (4, "ACCESSTOKEN"),
        ] {
            let params = serde_json::json!({
                "headers": {
                    header_key: "Bearer valid-token-123",
                    "content-type": "application/json"
                },
                "TOKEN": "ordinary-top-level",
                "arguments": {"TOKEN": "ordinary-argument"}
            });
            let request = AuthRequest {
                method: "tools/call",
                params: Some(&params),
                transport_authorization: None,
                request_id,
            };

            let auth = provider
                .authenticate(&ctx(), request)
                .expect("case-insensitive header alias is admitted");
            assert_eq!(auth.subject.as_deref(), Some("alice"));

            let mut stripped = Some(params);
            strip_recognized_access_credentials(&mut stripped);
            assert_eq!(
                stripped,
                Some(serde_json::json!({
                    "headers": {"content-type": "application/json"},
                    "TOKEN": "ordinary-top-level",
                    "arguments": {"TOKEN": "ordinary-argument"}
                }))
            );
        }

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 4);
    }

    #[test]
    fn auth_01_a_header_map_alias_case_insensitive_native_planted_negative() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token-123", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        });

        for (request_id, header_key) in [
            (1, "AUTH"),
            (2, "TOKEN"),
            (3, "ACCESS_TOKEN"),
            (4, "ACCESSTOKEN"),
        ] {
            let params = serde_json::json!({
                "headers": {
                    header_key: "Bearer valid-token-123",
                    "content-type": "application/json"
                },
                "TOKEN": "ordinary-top-level",
                "arguments": {"TOKEN": "ordinary-argument"}
            });
            let original = params.clone();
            let request = AuthRequest {
                method: "tools/call",
                params: Some(&params),
                transport_authorization: Some("Bearer valid-token-123"),
                request_id,
            };

            assert!(!request.credential_sources_are_admissible());
            assert!(request.has_multiple_credential_sources());
            let error = provider
                .authenticate(&ctx(), request)
                .expect_err("native and aliased header credentials must be rejected");
            assert_eq!(error.code, McpErrorCode::ResourceForbidden);
            assert_eq!(params, original);

            let mut stripped = Some(params);
            strip_recognized_access_credentials(&mut stripped);
            assert_eq!(
                stripped,
                Some(serde_json::json!({
                    "headers": {"content-type": "application/json"},
                    "TOKEN": "ordinary-top-level",
                    "arguments": {"TOKEN": "ordinary-argument"}
                }))
            );
        }

        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[test]
    fn nested_headers_malformed_value_rejected() {
        let params = serde_json::json!({
            "headers": {
                "AUTHORIZATION": "Bearer "
            }
        });
        let req = AuthRequest {
            method: "test",
            params: Some(&params),
            transport_authorization: None,
            request_id: 1,
        };

        assert!(req.access_token().is_none());
        assert!(!req.credential_sources_are_admissible());
        assert!(!req.has_multiple_credential_sources());
    }

    #[test]
    fn preservation_of_token_ordinary_tool_argument() {
        let params = serde_json::json!({
            "Token": "ordinary-arg-value",
            "arguments": {
                "Token": "another-arg"
            }
        });
        let req = AuthRequest {
            method: "tools/call",
            params: Some(&params),
            transport_authorization: Some("Bearer valid"),
            request_id: 1,
        };

        let token = req.access_token().expect("native token extracted");
        assert_eq!(token.token, "valid");
        assert!(!req.has_in_band_credential_source());
        assert!(!req.has_multiple_credential_sources());

        let mut stripped = Some(params.clone());
        strip_recognized_access_credentials(&mut stripped);
        assert_eq!(stripped, Some(params));
    }

    #[test]
    fn token_auth_provider_distinguishes_multiple_and_malformed_from_missing() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        });

        // 1. Positive: valid token proceeds to verifier
        let req_valid = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: Some("Bearer valid-token"),
            request_id: 1,
        };
        let auth = provider
            .authenticate(&ctx(), req_valid)
            .expect("valid credential succeeds");
        assert_eq!(auth.subject.as_deref(), Some("alice"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 2. Near-identical negative: absent credential returns default missing_token_error
        let req_absent = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let err_absent = provider
            .authenticate(&ctx(), req_absent)
            .expect_err("absent credential returns default missing error");
        assert_eq!(err_absent.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_absent.message, "Missing access token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 3. Near-identical negative: malformed native credential returns "Invalid access token"
        let req_malformed_native = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: Some("Bearer "),
            request_id: 1,
        };
        let err_malformed = provider
            .authenticate(&ctx(), req_malformed_native)
            .expect_err("malformed native credential must be rejected without verifier");
        assert_eq!(err_malformed.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_malformed.message, "Invalid access token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 4. Near-identical negative: multiple conflicting credentials (native + in-band)
        let params_in_band = serde_json::json!({
            "headers": {
                "Authorization": "Bearer valid-token"
            }
        });
        let req_multiple = AuthRequest {
            method: "tools/call",
            params: Some(&params_in_band),
            transport_authorization: Some("Bearer valid-token"),
            request_id: 1,
        };
        let err_multiple = provider
            .authenticate(&ctx(), req_multiple)
            .expect_err("multiple conflicting credentials must be rejected without verifier");
        assert_eq!(err_multiple.code, McpErrorCode::ResourceForbidden);
        assert_eq!(
            err_multiple.message,
            "Multiple conflicting credential sources"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 5. Near-identical negative: malformed in-band credential
        let params_malformed = serde_json::json!({
            "headers": {
                "Authorization": "Bearer "
            }
        });
        let req_malformed_in_band = AuthRequest {
            method: "tools/call",
            params: Some(&params_malformed),
            transport_authorization: None,
            request_id: 1,
        };
        let err_malformed_in_band = provider
            .authenticate(&ctx(), req_malformed_in_band)
            .expect_err("malformed in-band credential must be rejected without verifier");
        assert_eq!(err_malformed_in_band.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_malformed_in_band.message, "Invalid access token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 6. Near-identical negative: duplicate in-band case-variant headers
        let params_duplicate_headers = serde_json::json!({
            "headers": {
                "authorization": "Bearer valid-token",
                "Authorization": "Bearer valid-token"
            }
        });
        let req_duplicate_headers = AuthRequest {
            method: "tools/call",
            params: Some(&params_duplicate_headers),
            transport_authorization: None,
            request_id: 1,
        };
        let err_duplicate = provider
            .authenticate(&ctx(), req_duplicate_headers)
            .expect_err("duplicate in-band header credentials must be rejected without verifier");
        assert_eq!(err_duplicate.code, McpErrorCode::ResourceForbidden);
        assert_eq!(
            err_duplicate.message,
            "Multiple conflicting credential sources"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[test]
    fn token_auth_provider_preserves_custom_missing_error_only_when_empty() {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let verifier =
            StaticTokenVerifier::new([("valid-token", AuthContext::with_subject("alice"))])
                .expect("valid verifier configuration");
        let provider = TokenAuthProvider::new(CountingVerifier {
            inner: verifier,
            calls: Arc::clone(&calls),
        })
        .with_missing_token_error(auth_error("custom-missing-token"));

        // 1. Positive: valid token proceeds to verifier
        let req_valid = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: Some("Bearer valid-token"),
            request_id: 1,
        };
        let auth = provider
            .authenticate(&ctx(), req_valid)
            .expect("valid credential succeeds");
        assert_eq!(auth.subject.as_deref(), Some("alice"));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 2. Near-identical negative: absent credential preserves custom missing_token_error
        let req_absent = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let err_absent = provider
            .authenticate(&ctx(), req_absent)
            .expect_err("absent credential returns custom missing error");
        assert_eq!(err_absent.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_absent.message, "custom-missing-token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 3. Near-identical negative: malformed native credential returns "Invalid access token" (not custom missing)
        let req_malformed_native = AuthRequest {
            method: "tools/call",
            params: None,
            transport_authorization: Some("Bearer "),
            request_id: 1,
        };
        let err_malformed = provider
            .authenticate(&ctx(), req_malformed_native)
            .expect_err("malformed native credential must be rejected without verifier");
        assert_eq!(err_malformed.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_malformed.message, "Invalid access token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 4. Near-identical negative: multiple conflicting credentials (native + in-band) (not custom missing)
        let params_in_band = serde_json::json!({
            "headers": {
                "Authorization": "Bearer valid-token"
            }
        });
        let req_multiple = AuthRequest {
            method: "tools/call",
            params: Some(&params_in_band),
            transport_authorization: Some("Bearer valid-token"),
            request_id: 1,
        };
        let err_multiple = provider
            .authenticate(&ctx(), req_multiple)
            .expect_err("multiple conflicting credentials must be rejected without verifier");
        assert_eq!(err_multiple.code, McpErrorCode::ResourceForbidden);
        assert_eq!(
            err_multiple.message,
            "Multiple conflicting credential sources"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 5. Near-identical negative: malformed in-band credential (not custom missing)
        let params_malformed = serde_json::json!({
            "headers": {
                "Authorization": "Bearer "
            }
        });
        let req_malformed_in_band = AuthRequest {
            method: "tools/call",
            params: Some(&params_malformed),
            transport_authorization: None,
            request_id: 1,
        };
        let err_malformed_in_band = provider
            .authenticate(&ctx(), req_malformed_in_band)
            .expect_err("malformed in-band credential must be rejected without verifier");
        assert_eq!(err_malformed_in_band.code, McpErrorCode::ResourceForbidden);
        assert_eq!(err_malformed_in_band.message, "Invalid access token");
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);

        // 6. Near-identical negative: duplicate in-band case-variant headers (not custom missing)
        let params_duplicate_headers = serde_json::json!({
            "headers": {
                "authorization": "Bearer valid-token",
                "Authorization": "Bearer valid-token"
            }
        });
        let req_duplicate_headers = AuthRequest {
            method: "tools/call",
            params: Some(&params_duplicate_headers),
            transport_authorization: None,
            request_id: 1,
        };
        let err_duplicate = provider
            .authenticate(&ctx(), req_duplicate_headers)
            .expect_err("duplicate in-band header credentials must be rejected without verifier");
        assert_eq!(err_duplicate.code, McpErrorCode::ResourceForbidden);
        assert_eq!(
            err_duplicate.message,
            "Multiple conflicting credential sources"
        );
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    fn registry_snapshot(
        verifier: &StaticTokenVerifier,
    ) -> HashMap<Sha256Digest, (String, Option<Instant>)> {
        verifier
            .tokens
            .read()
            .expect("registry lock")
            .iter()
            .map(|(digest, entry)| {
                (
                    *digest,
                    (
                        serde_json::to_string(&entry.context).expect("auth facts"),
                        entry.expires_at,
                    ),
                )
            })
            .collect()
    }

    fn authenticate_header(
        provider: &TokenAuthProvider,
        authorization: &str,
    ) -> McpResult<AuthContext> {
        provider.authenticate(
            &ctx(),
            AuthRequest {
                method: "tools/call",
                params: None,
                transport_authorization: Some(authorization),
                request_id: 1,
            },
        )
    }

    #[test]
    fn static_token_rotation_is_visible_to_installed_provider_clones() {
        let mut facts = AuthContext::with_subject("rotation-owner");
        facts.scopes = vec!["read".to_owned()];
        facts.claims = Some(serde_json::json!({"policy_revision": 7}));
        let verifier = StaticTokenVerifier::new([("old-secret", facts.clone())])
            .unwrap()
            .with_allowed_schemes(["Bearer"])
            .unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(3_600);
        assert!(verifier.expire_token_at("old-secret", deadline).unwrap());
        let controls = verifier.clone();
        let provider = TokenAuthProvider::new(verifier);
        let peer = provider.clone();
        assert!(authenticate_header(&provider, "Bearer old-secret").is_ok());

        controls.rotate_token("old-secret", "new-secret").unwrap();
        for installed in [&provider, &peer] {
            assert!(authenticate_header(installed, "Bearer old-secret").is_err());
            let actual = authenticate_header(installed, "Bearer new-secret").unwrap();
            assert_eq!(
                serde_json::to_value(actual).unwrap(),
                serde_json::to_value(&facts).unwrap()
            );
            assert!(authenticate_header(installed, "Basic new-secret").is_err());
        }
        let snapshot = registry_snapshot(&controls);
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot.values().next().unwrap().1, Some(deadline));
        let debug = format!("{controls:?}");
        assert!(!debug.contains("old-secret"));
        assert!(!debug.contains("new-secret"));
        assert!(!debug.contains("rotation-owner"));
    }

    #[test]
    fn static_token_rotation_refusals_preserve_the_registry() {
        let verifier = StaticTokenVerifier::new([
            ("alpha-secret", AuthContext::with_subject("alice")),
            ("beta-secret", AuthContext::with_subject("bob")),
        ])
        .unwrap();
        let before = registry_snapshot(&verifier);
        for (old, new) in [
            ("missing-secret", "fresh-secret"),
            ("alpha-secret", "beta-secret"),
            ("alpha-secret", "alpha-secret"),
            ("alpha-secret", "invalid:secret"),
            ("invalid:secret", "fresh-secret"),
        ] {
            let error = verifier.rotate_token(old, new).unwrap_err();
            assert_eq!(error.code, McpErrorCode::ResourceForbidden);
            assert!(!error.message.contains(old));
            assert!(!error.message.contains(new));
            assert_eq!(registry_snapshot(&verifier), before);
        }
        verifier
            .expire_token_at("alpha-secret", Instant::now())
            .unwrap();
        let expired = registry_snapshot(&verifier);
        assert!(
            verifier
                .rotate_token("alpha-secret", "fresh-secret")
                .is_err()
        );
        assert_eq!(registry_snapshot(&verifier), expired);
        let provider = TokenAuthProvider::new(verifier);
        assert!(authenticate_header(&provider, "Bearer alpha-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer beta-secret").is_ok());
        assert!(authenticate_header(&provider, "Bearer fresh-secret").is_err());
    }

    #[test]
    fn static_token_replacement_refusals_preserve_every_active_entry() {
        let verifier =
            StaticTokenVerifier::new([("existing-secret", AuthContext::with_subject("owner"))])
                .unwrap();
        let before = registry_snapshot(&verifier);
        for replacement in [
            vec![],
            vec![("new-secret", AuthContext::anonymous())],
            vec![
                ("new-secret", AuthContext::with_subject("first")),
                ("new-secret", AuthContext::with_subject("second")),
            ],
            vec![
                ("new-secret", AuthContext::with_subject("first")),
                ("bad:secret", AuthContext::with_subject("second")),
            ],
        ] {
            assert!(verifier.replace_tokens(replacement).is_err());
            assert_eq!(registry_snapshot(&verifier), before);
        }
        let excessive = (0..=MAX_STATIC_TOKEN_ENTRIES)
            .map(|index| (format!("new-{index}"), AuthContext::with_subject("owner")));
        assert!(verifier.replace_tokens(excessive).is_err());
        assert_eq!(registry_snapshot(&verifier), before);
        let provider = TokenAuthProvider::new(verifier);
        assert!(authenticate_header(&provider, "Bearer existing-secret").is_ok());
        assert!(authenticate_header(&provider, "Bearer new-secret").is_err());
    }

    #[test]
    fn static_token_replacement_updates_facts_without_reviving_retained_secrets() {
        let verifier = StaticTokenVerifier::new([
            ("retained-secret", AuthContext::with_subject("owner")),
            ("removed-secret", AuthContext::with_subject("other")),
        ])
        .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        let mut revised = AuthContext::with_subject("owner");
        revised.scopes = vec!["read".to_owned()];
        verifier
            .expire_token_at("retained-secret", Instant::now())
            .unwrap();
        verifier
            .replace_tokens([
                ("retained-secret", revised.clone()),
                ("added-secret", revised.clone()),
            ])
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer retained-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer removed-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer fresh-secret").is_err());
        let added = authenticate_header(&provider, "Bearer added-secret").unwrap();
        assert_eq!(added.scopes, revised.scopes);
        assert_eq!(added.subject, revised.subject);
        assert_eq!(registry_snapshot(&verifier).len(), 2);
    }

    #[test]
    fn static_token_expiry_is_inclusive_monotonic_and_cannot_be_extended() {
        let verifier =
            StaticTokenVerifier::new([("expiring-secret", AuthContext::with_subject("owner"))])
                .unwrap();
        let token = AccessToken::parse("Bearer expiring-secret").unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(3_600);
        assert!(
            verifier
                .expire_token_at("expiring-secret", deadline)
                .unwrap()
        );
        assert!(
            verifier
                .verify_with_clock(&token, || deadline - std::time::Duration::from_nanos(1))
                .is_ok()
        );
        assert!(verifier.verify_with_clock(&token, || deadline).is_err());
        assert!(
            verifier
                .verify_with_clock(&token, || deadline + std::time::Duration::from_nanos(1))
                .is_err()
        );
        let before = registry_snapshot(&verifier);
        assert!(
            verifier
                .expire_token_at(
                    "expiring-secret",
                    deadline + std::time::Duration::from_secs(1)
                )
                .unwrap()
        );
        assert_eq!(registry_snapshot(&verifier), before);
        assert!(
            !verifier
                .expire_token_at("unknown-secret", deadline)
                .unwrap()
        );
        assert_eq!(registry_snapshot(&verifier), before);

        let provider = TokenAuthProvider::new(verifier.clone());
        assert!(authenticate_header(&provider, "Bearer expiring-secret").is_ok());
        verifier
            .expire_token_at("expiring-secret", Instant::now())
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer expiring-secret").is_err());
        verifier
            .expire_token_at("expiring-secret", deadline)
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer expiring-secret").is_err());
    }

    #[test]
    fn static_token_revocation_and_deny_all_reach_installed_providers() {
        let verifier = StaticTokenVerifier::new([
            ("first-secret", AuthContext::with_subject("first")),
            ("second-secret", AuthContext::with_subject("second")),
        ])
        .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        assert!(authenticate_header(&provider, "Bearer first-secret").is_ok());
        let before = registry_snapshot(&verifier);
        assert!(verifier.revoke_token("bad:secret").is_err());
        assert_eq!(registry_snapshot(&verifier), before);
        assert!(!verifier.revoke_token("unknown-secret").unwrap());
        assert_eq!(registry_snapshot(&verifier), before);
        assert!(verifier.revoke_token("first-secret").unwrap());
        assert!(!verifier.revoke_token("first-secret").unwrap());
        assert!(authenticate_header(&provider, "Bearer first-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer second-secret").is_ok());
        assert_eq!(verifier.revoke_all().unwrap(), 1);
        assert_eq!(verifier.revoke_all().unwrap(), 0);
        assert!(authenticate_header(&provider, "Bearer second-secret").is_err());
        verifier
            .replace_tokens([("fresh-secret", AuthContext::with_subject("fresh"))])
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer fresh-secret").is_ok());
        assert!(authenticate_header(&provider, "Bearer first-secret").is_err());
    }

    #[test]
    fn static_token_rotation_works_at_the_registry_capacity_limit() {
        let verifier = StaticTokenVerifier::new((0..MAX_STATIC_TOKEN_ENTRIES).map(|index| {
            (
                format!("secret-{index}"),
                AuthContext::with_subject(format!("owner-{index}")),
            )
        }))
        .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        verifier
            .rotate_token("secret-0", "replacement-secret")
            .unwrap();
        assert_eq!(registry_snapshot(&verifier).len(), MAX_STATIC_TOKEN_ENTRIES);
        assert!(authenticate_header(&provider, "Bearer secret-0").is_err());
        assert_eq!(
            authenticate_header(&provider, "Bearer replacement-secret")
                .unwrap()
                .subject
                .as_deref(),
            Some("owner-0")
        );
        assert!(authenticate_header(&provider, "Bearer secret-1").is_ok());
    }

    #[test]
    fn poisoned_static_token_registry_fails_closed_without_repair_or_secret_leak() {
        let verifier =
            StaticTokenVerifier::new([("poison-secret", AuthContext::with_subject("owner"))])
                .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        assert!(authenticate_header(&provider, "Bearer poison-secret").is_ok());
        let planted = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = verifier.tokens.write().unwrap();
            panic!("planted registry poison");
        }));
        assert!(planted.is_err());
        let error = authenticate_header(&provider, "Bearer poison-secret").unwrap_err();
        assert_eq!(error.code, McpErrorCode::ResourceForbidden);
        assert!(!error.message.contains("poison-secret"));
        assert!(verifier.revoke_token("poison-secret").is_err());
        assert!(verifier.revoke_all().is_err());
        assert!(
            verifier
                .expire_token_at("poison-secret", Instant::now())
                .is_err()
        );
        assert!(
            verifier
                .rotate_token("poison-secret", "new-secret")
                .is_err()
        );
        assert!(
            verifier
                .replace_tokens([("new-secret", AuthContext::with_subject("owner"))])
                .is_err()
        );
        assert!(authenticate_header(&provider, "Bearer new-secret").is_err());
        let debug = format!("{verifier:?}");
        assert!(!debug.contains("poison-secret"));
        assert!(!debug.contains("new-secret"));
    }

    #[test]
    fn static_token_initial_expiry_applies_to_the_first_authentication() {
        use std::time::Duration;

        let now = Instant::now();
        let deadline = now + Duration::from_secs(3_600);
        let verifier = StaticTokenVerifier::new_with_expiry([
            (
                "expired-secret",
                AuthContext::with_subject("expired"),
                Some(now),
            ),
            (
                "active-secret",
                AuthContext::with_subject("active"),
                Some(deadline),
            ),
            (
                "unbounded-secret",
                AuthContext::with_subject("unbounded"),
                None,
            ),
        ])
        .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        assert!(authenticate_header(&provider, "Bearer expired-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer active-secret").is_ok());
        assert!(authenticate_header(&provider, "Bearer unbounded-secret").is_ok());
        let token = AccessToken::parse("Bearer active-secret").unwrap();
        assert!(verifier.verify_with_clock(&token, || deadline).is_err());
    }

    #[test]
    fn static_token_reload_admits_deadlines_atomically_and_only_shortens_existing_limits() {
        use std::time::Duration;

        let now = Instant::now();
        let deadline = now + Duration::from_secs(3_600);
        let owner = AuthContext::with_subject("owner");
        let verifier = StaticTokenVerifier::new_with_expiry([
            ("retained-secret", owner.clone(), Some(deadline)),
            ("removed-secret", owner.clone(), None),
        ])
        .unwrap();
        let provider = TokenAuthProvider::new(verifier.clone());
        let before = registry_snapshot(&verifier);
        assert!(
            verifier
                .replace_tokens_with_expiry([
                    ("fresh-secret", owner.clone(), Some(deadline)),
                    ("fresh-secret", owner.clone(), None),
                ])
                .is_err()
        );
        assert_eq!(registry_snapshot(&verifier), before);
        verifier
            .replace_tokens_with_expiry([
                (
                    "retained-secret",
                    owner.clone(),
                    Some(deadline + Duration::from_secs(1)),
                ),
                ("expired-secret", owner.clone(), Some(now)),
                ("fresh-secret", owner.clone(), Some(deadline)),
            ])
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer retained-secret").is_ok());
        assert!(authenticate_header(&provider, "Bearer removed-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer expired-secret").is_err());
        assert!(authenticate_header(&provider, "Bearer fresh-secret").is_ok());
        let retained = AccessToken::parse("Bearer retained-secret").unwrap();
        assert!(verifier.verify_with_clock(&retained, || deadline).is_err());
        verifier
            .replace_tokens_with_expiry([("retained-secret", owner.clone(), Some(now))])
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer retained-secret").is_err());
        verifier
            .replace_tokens([("retained-secret", owner)])
            .unwrap();
        assert!(authenticate_header(&provider, "Bearer retained-secret").is_err());
    }
}
