//! MCP session management.

use std::collections::HashSet;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use fastmcp_core::logging::{debug, targets, warn};
use fastmcp_core::{McpContext, SessionState, Sha256Digest, sha256_bounded};
use fastmcp_protocol::{
    ClientCapabilities, ClientInfo, JsonRpcRequest, LogLevel, ResourceUpdatedNotificationParams,
    ServerCapabilities, ServerInfo,
};

use crate::NotificationSender;

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

const fn checked_session_id_successor(current: u64) -> Option<u64> {
    current.checked_add(1)
}

fn next_session_id() -> u64 {
    NEXT_SESSION_ID
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            checked_session_id_successor(current)
        })
        .unwrap_or_else(|_| {
            // `Session::new` is infallible, so there is no safe API-level way
            // to report exhaustion. Reusing an ID would alias cancellation and
            // request-ownership domains across live sessions; fail closed
            // instead.
            panic!("process-local MCP session identity space exhausted")
        })
}

/// Default maximum number of retained resource subscriptions per session.
pub(crate) const MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION: usize = 32;
/// Default maximum aggregate UTF-8 bytes retained for resource subscription URIs.
pub(crate) const MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION: usize = 4 * 1024 * 1024;

/// Maximum peer-controlled URI bytes hashed for one log correlation label.
const RESOURCE_LOG_HASH_INPUT_LIMIT: usize = 4 * 1024;
const RESOURCE_LOG_DIGEST_PREFIX_BYTES: usize = 8;

#[derive(Clone, Copy)]
struct SafeResourceLogLabel {
    byte_len: usize,
    hashed_bytes: usize,
    digest_prefix: [u8; RESOURCE_LOG_DIGEST_PREFIX_BYTES],
}

impl fmt::Display for SafeResourceLogLabel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bytes={},sha256_prefix=", self.byte_len)?;
        for byte in self.digest_prefix {
            write!(f, "{byte:02x}")?;
        }
        if self.hashed_bytes < self.byte_len {
            write!(f, ",hashed_prefix_bytes={}", self.hashed_bytes)?;
        }
        Ok(())
    }
}

fn safe_resource_log_label(uri: &str) -> SafeResourceLogLabel {
    let bytes = uri.as_bytes();
    let hashed_bytes = bytes.len().min(RESOURCE_LOG_HASH_INPUT_LIMIT);
    let bounded_prefix = &bytes[..hashed_bytes];
    let mut digest_prefix = [0_u8; RESOURCE_LOG_DIGEST_PREFIX_BYTES];
    if let Ok(digest) = sha256_bounded(bounded_prefix, RESOURCE_LOG_HASH_INPUT_LIMIT) {
        digest_prefix.copy_from_slice(&digest.as_bytes()[..RESOURCE_LOG_DIGEST_PREFIX_BYTES]);
    }

    SafeResourceLogLabel {
        byte_len: bytes.len(),
        hashed_bytes,
        digest_prefix,
    }
}

/// Successful result of admitting a resource subscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionAdmission {
    /// A new URI was retained by the session.
    Accepted,
    /// The URI was already retained; subscribe is idempotent.
    Duplicate,
}

/// Fail-closed rejection of a resource-subscription admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionAdmissionError {
    /// Retaining the URI would exceed a per-session bound.
    CapacityExceeded,
    /// The request context is cancelled, expired, or no longer owns a live lease.
    RequestNotLive,
}

/// Successful result of an idempotent resource unsubscription.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionRemoval {
    /// A retained URI was removed.
    Removed,
    /// The URI was not retained, so no mutation was necessary.
    NotSubscribed,
}

/// Fail-closed rejection of a resource-unsubscription mutation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubscriptionRemovalError {
    /// The request context is cancelled, expired, or no longer owns a live lease.
    RequestNotLive,
}

/// Exact initialization fields restored when an initialize response does not
/// reach its commit boundary.
///
/// Session dispatch is serialized, so retaining one private snapshot while a
/// response is finalized provides transaction-like rollback without exposing
/// peer identity or capability contents outside the session module.
#[derive(Clone)]
pub(crate) struct InitializationSnapshot {
    initialized: bool,
    client_info: Option<ClientInfo>,
    client_capabilities: Option<ClientCapabilities>,
    protocol_version: Option<String>,
}

/// Shared, credential-free binding between a connection session and the
/// verified principal admitted on its first authenticated frame.
#[derive(Clone, Default)]
pub(crate) struct SessionPrincipalBinding {
    fingerprint: Arc<Mutex<Option<Sha256Digest>>>,
}

impl fmt::Debug for SessionPrincipalBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SessionPrincipalBinding")
            .field("bound", &self.is_bound_for_debug())
            .finish()
    }
}

impl SessionPrincipalBinding {
    /// Binds an unclaimed session or verifies that a later request carries the
    /// same principal. A mismatch fails closed without replacing the owner.
    pub(crate) fn bind_or_verify(&self, fingerprint: Sha256Digest) -> bool {
        let mut bound = self
            .fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match *bound {
            Some(existing) => existing == fingerprint,
            None => {
                *bound = Some(fingerprint);
                true
            }
        }
    }

    /// Verifies a control frame against an owner established by an ordinary
    /// dispatched request. Control frames must never win an admission-order
    /// race and claim an otherwise unbound session.
    pub(crate) fn verify_existing(&self, fingerprint: Sha256Digest) -> bool {
        self.fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some_and(|existing| existing == fingerprint)
    }

    fn is_bound_for_debug(&self) -> bool {
        self.fingerprint
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
}

/// An MCP session between client and server.
///
/// Tracks the state of an initialized MCP connection.
/// Debug output is deliberately redacted to safe counts and presence booleans;
/// it does not render client/server identity, protocol text, subscription URIs,
/// or session-state contents.
pub struct Session {
    /// Process-local identity used to bind request ownership and cancellation.
    id: u64,
    /// Whether the session has been initialized.
    initialized: bool,
    /// Client info from initialization.
    client_info: Option<ClientInfo>,
    /// Client capabilities from initialization.
    client_capabilities: Option<ClientCapabilities>,
    /// Server info.
    server_info: ServerInfo,
    /// Server capabilities.
    server_capabilities: ServerCapabilities,
    /// Negotiated protocol version.
    protocol_version: Option<String>,
    /// Resource subscriptions for this session.
    resource_subscriptions: HashSet<String>,
    /// Aggregate UTF-8 bytes retained by unique resource subscription URIs.
    resource_subscription_bytes: usize,
    /// Session-scoped log level for log notifications.
    log_level: Option<LogLevel>,
    /// Per-session state storage.
    state: SessionState,
    /// Stable verified owner of session-scoped state and control operations.
    principal_binding: SessionPrincipalBinding,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("initialized", &self.initialized)
            .field("has_client_info", &self.client_info.is_some())
            .field(
                "has_client_capabilities",
                &self.client_capabilities.is_some(),
            )
            .field("has_protocol_version", &self.protocol_version.is_some())
            .field(
                "resource_subscription_count",
                &self.resource_subscriptions.len(),
            )
            .field(
                "resource_subscription_bytes",
                &self.resource_subscription_bytes,
            )
            .field("has_log_level", &self.log_level.is_some())
            .field("state_entry_count", &self.state.len())
            .field(
                "principal_bound",
                &self.principal_binding.is_bound_for_debug(),
            )
            .finish()
    }
}

impl Session {
    /// Creates a new uninitialized session.
    #[must_use]
    pub fn new(server_info: ServerInfo, server_capabilities: ServerCapabilities) -> Self {
        Self {
            id: next_session_id(),
            initialized: false,
            client_info: None,
            client_capabilities: None,
            server_info,
            server_capabilities,
            protocol_version: None,
            resource_subscriptions: HashSet::new(),
            resource_subscription_bytes: 0,
            log_level: None,
            state: SessionState::new(),
            principal_binding: SessionPrincipalBinding::default(),
        }
    }

    /// Returns the process-local identity of this connection session.
    #[must_use]
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// Returns a reference to the session state.
    ///
    /// Session state persists across requests within this session and can be
    /// used to store handler-specific data.
    #[must_use]
    pub fn state(&self) -> &SessionState {
        &self.state
    }

    /// Returns the credential-free shared principal binding for this session.
    pub(crate) fn principal_binding(&self) -> SessionPrincipalBinding {
        self.principal_binding.clone()
    }

    /// Returns whether the session has been initialized.
    #[must_use]
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Initializes the session with client info.
    pub fn initialize(
        &mut self,
        client_info: ClientInfo,
        client_capabilities: ClientCapabilities,
        protocol_version: String,
    ) {
        self.client_info = Some(client_info);
        self.client_capabilities = Some(client_capabilities);
        self.protocol_version = Some(protocol_version);
        self.initialized = true;
    }

    /// Captures every field mutated by [`Self::initialize`].
    pub(crate) fn initialization_snapshot(&self) -> InitializationSnapshot {
        InitializationSnapshot {
            initialized: self.initialized,
            client_info: self.client_info.clone(),
            client_capabilities: self.client_capabilities.clone(),
            protocol_version: self.protocol_version.clone(),
        }
    }

    /// Restores an initialization snapshot after response finalization fails.
    pub(crate) fn restore_initialization(&mut self, snapshot: InitializationSnapshot) {
        self.initialized = snapshot.initialized;
        self.client_info = snapshot.client_info;
        self.client_capabilities = snapshot.client_capabilities;
        self.protocol_version = snapshot.protocol_version;
    }

    /// Returns the client info if initialized.
    #[must_use]
    pub fn client_info(&self) -> Option<&ClientInfo> {
        self.client_info.as_ref()
    }

    /// Returns the client capabilities if initialized.
    #[must_use]
    pub fn client_capabilities(&self) -> Option<&ClientCapabilities> {
        self.client_capabilities.as_ref()
    }

    /// Returns the server info.
    #[must_use]
    pub fn server_info(&self) -> &ServerInfo {
        &self.server_info
    }

    /// Returns the server capabilities.
    #[must_use]
    pub fn server_capabilities(&self) -> &ServerCapabilities {
        &self.server_capabilities
    }

    /// Returns the negotiated protocol version.
    #[must_use]
    pub fn protocol_version(&self) -> Option<&str> {
        self.protocol_version.as_deref()
    }

    /// Subscribes to a resource URI for this session.
    ///
    /// Duplicate subscriptions are idempotent. New subscriptions that would
    /// exceed the per-session count or aggregate UTF-8 byte limit are rejected
    /// fail-closed and are not retained. A non-live request cannot mutate the
    /// session.
    pub(crate) fn subscribe_resource(
        &mut self,
        ctx: &McpContext,
        uri: String,
    ) -> Result<SubscriptionAdmission, SubscriptionAdmissionError> {
        if ctx.ensure_live().is_err() {
            return Err(SubscriptionAdmissionError::RequestNotLive);
        }

        // Reject an individually impossible candidate before hashing it for
        // duplicate lookup. A URI larger than the aggregate byte cap can
        // never be retained, even by an otherwise empty session.
        if uri.len() > MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION {
            warn!(
                target: targets::SESSION,
                "resource subscription rejected by session limits; subscription_count={}; subscription_count_limit={}; retained_uri_bytes={}; retained_uri_bytes_limit={}; candidate_uri={}",
                self.resource_subscriptions.len(),
                MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION,
                self.resource_subscription_bytes,
                MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION,
                safe_resource_log_label(&uri),
            );
            return Err(SubscriptionAdmissionError::CapacityExceeded);
        }

        if self.resource_subscriptions.contains(&uri) {
            return Ok(SubscriptionAdmission::Duplicate);
        }

        let prospective_bytes = self.resource_subscription_bytes.checked_add(uri.len());
        if self.resource_subscriptions.len() >= MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION
            || prospective_bytes
                .is_none_or(|bytes| bytes > MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION)
        {
            warn!(
                target: targets::SESSION,
                "resource subscription rejected by session limits; subscription_count={}; subscription_count_limit={}; retained_uri_bytes={}; retained_uri_bytes_limit={}; candidate_uri={}",
                self.resource_subscriptions.len(),
                MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION,
                self.resource_subscription_bytes,
                MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION,
                safe_resource_log_label(&uri),
            );
            return Err(SubscriptionAdmissionError::CapacityExceeded);
        }

        let Some(prospective_bytes) = prospective_bytes else {
            return Err(SubscriptionAdmissionError::CapacityExceeded);
        };

        // Reserve before the commit boundary so peer-driven growth reports a
        // bounded admission failure instead of reaching HashSet's infallible
        // allocation path and potentially panicking the session dispatcher.
        if self.resource_subscriptions.try_reserve(1).is_err() {
            warn!(
                target: targets::SESSION,
                "resource subscription rejected because bounded storage could not be reserved; subscription_count={}; subscription_count_limit={}; retained_uri_bytes={}; retained_uri_bytes_limit={}; candidate_uri={}",
                self.resource_subscriptions.len(),
                MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION,
                self.resource_subscription_bytes,
                MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION,
                safe_resource_log_label(&uri),
            );
            return Err(SubscriptionAdmissionError::CapacityExceeded);
        }

        // Recheck immediately before the synchronous commit so cancellation
        // observed during admission cannot retain a new subscription.
        if ctx.ensure_live().is_err() {
            return Err(SubscriptionAdmissionError::RequestNotLive);
        }

        if self.resource_subscriptions.insert(uri) {
            self.resource_subscription_bytes = prospective_bytes;
            Ok(SubscriptionAdmission::Accepted)
        } else {
            Ok(SubscriptionAdmission::Duplicate)
        }
    }

    /// Idempotently unsubscribes from a resource URI for this session.
    pub(crate) fn unsubscribe_resource(
        &mut self,
        ctx: &McpContext,
        uri: &str,
    ) -> Result<SubscriptionRemoval, SubscriptionRemovalError> {
        self.unsubscribe_resource_with_precommit(ctx, uri, || {})
    }

    fn unsubscribe_resource_with_precommit(
        &mut self,
        ctx: &McpContext,
        uri: &str,
        before_commit: impl FnOnce(),
    ) -> Result<SubscriptionRemoval, SubscriptionRemovalError> {
        if ctx.ensure_live().is_err() {
            return Err(SubscriptionRemovalError::RequestNotLive);
        }

        // Such a URI cannot have passed subscription admission, so preserve
        // idempotent success without hashing an unretainable peer string.
        if uri.len() > MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION {
            return Ok(SubscriptionRemoval::NotSubscribed);
        }

        let subscribed = self.resource_subscriptions.contains(uri);
        before_commit();

        // Hashing a retained peer URI can be substantial at the configured
        // bound. Recheck immediately before the synchronous removal so
        // cancellation observed during that work cannot commit a mutation.
        if ctx.ensure_live().is_err() {
            return Err(SubscriptionRemovalError::RequestNotLive);
        }

        if !subscribed {
            return Ok(SubscriptionRemoval::NotSubscribed);
        }

        if let Some(removed) = self.resource_subscriptions.take(uri) {
            self.resource_subscription_bytes = self
                .resource_subscription_bytes
                .saturating_sub(removed.len());
            Ok(SubscriptionRemoval::Removed)
        } else {
            Ok(SubscriptionRemoval::NotSubscribed)
        }
    }

    /// Rolls back a newly admitted subscription when request finalization fails.
    pub(crate) fn rollback_resource_subscription(&mut self, uri: &str) {
        if let Some(removed) = self.resource_subscriptions.take(uri) {
            self.resource_subscription_bytes = self
                .resource_subscription_bytes
                .saturating_sub(removed.len());
        }
    }

    /// Restores a removed subscription when request finalization fails.
    pub(crate) fn restore_resource_subscription(&mut self, uri: String) {
        let uri_len = uri.len();
        if self.resource_subscriptions.insert(uri) {
            self.resource_subscription_bytes = self
                .resource_subscription_bytes
                .checked_add(uri_len)
                .expect("restoring a previously retained subscription cannot overflow");
        }
        debug_assert!(self.resource_subscriptions.len() <= MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION);
        debug_assert!(
            self.resource_subscription_bytes <= MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION
        );
    }

    /// Returns true if this session is subscribed to the given resource URI.
    #[must_use]
    pub fn is_resource_subscribed(&self, uri: &str) -> bool {
        self.resource_subscriptions.contains(uri)
    }

    /// Returns the resource URIs this session currently subscribes to.
    pub fn subscribed_resource_uris(&self) -> impl Iterator<Item = &str> {
        self.resource_subscriptions.iter().map(String::as_str)
    }

    /// Sets the session log level for log notifications.
    pub fn set_log_level(&mut self, level: LogLevel) {
        self.log_level = Some(level);
    }

    /// Restores the exact pre-request log-level state, including `None`.
    pub(crate) fn restore_log_level(&mut self, level: Option<LogLevel>) {
        self.log_level = level;
    }

    /// Returns the current session log level for log notifications.
    #[must_use]
    pub fn log_level(&self) -> Option<LogLevel> {
        self.log_level
    }

    /// Returns whether the client supports sampling (LLM completions).
    #[must_use]
    pub fn supports_sampling(&self) -> bool {
        self.client_capabilities
            .as_ref()
            .is_some_and(|caps| caps.sampling.is_some())
    }

    /// Returns whether the client supports elicitation (user input requests).
    #[must_use]
    pub fn supports_elicitation(&self) -> bool {
        self.client_capabilities
            .as_ref()
            .is_some_and(|caps| caps.elicitation.is_some())
    }

    /// Returns whether the client supports roots listing.
    #[must_use]
    pub fn supports_roots(&self) -> bool {
        self.client_capabilities
            .as_ref()
            .is_some_and(|caps| caps.roots.is_some())
    }

    /// Sends a resource updated notification if the session is subscribed.
    ///
    /// Returns true if a notification was sent.
    pub fn notify_resource_updated(&self, uri: &str, sender: &NotificationSender) -> bool {
        if !self.is_resource_subscribed(uri) {
            return false;
        }

        let safe_uri = safe_resource_log_label(uri);
        let params = ResourceUpdatedNotificationParams {
            uri: uri.to_string(),
        };
        let payload = match serde_json::to_value(params) {
            Ok(value) => value,
            Err(err) => {
                warn!(
                    target: targets::SESSION,
                    "failed to serialize resource update; uri={}: {}",
                    safe_uri,
                    err
                );
                return false;
            }
        };

        debug!(
            target: targets::SESSION,
            "sending resource update notification; uri={}",
            safe_uri
        );
        let notification =
            JsonRpcRequest::notification("notifications/resources/updated", Some(payload));
        if crate::catch_extension_unwind(|| sender(notification)).is_err() {
            warn!(
                target: targets::SESSION,
                "resource update notification sender terminated unexpectedly; uri={}; detail=panic_payload_redacted",
                safe_uri
            );
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::Cx;
    use fastmcp_protocol::{ElicitationCapability, RootsCapability, SamplingCapability};
    use std::sync::{Arc, Mutex};

    fn make_server_info() -> ServerInfo {
        ServerInfo {
            name: "test".to_string(),
            version: "1.0".to_string(),
        }
    }

    fn make_client_info() -> ClientInfo {
        ClientInfo {
            name: "test-client".to_string(),
            version: "1.0".to_string(),
        }
    }

    fn make_session() -> Session {
        Session::new(make_server_info(), ServerCapabilities::default())
    }

    fn live_context() -> McpContext {
        McpContext::new(Cx::for_testing(), 1)
    }

    fn admit_resource(session: &mut Session, uri: impl Into<String>) -> SubscriptionAdmission {
        session
            .subscribe_resource(&live_context(), uri.into())
            .expect("test subscription should be admitted")
    }

    fn remove_resource(session: &mut Session, uri: &str) -> SubscriptionRemoval {
        session
            .unsubscribe_resource(&live_context(), uri)
            .expect("live test unsubscription should succeed")
    }

    // ── New session initial state ────────────────────────────────────

    #[test]
    fn new_session_is_not_initialized() {
        let session = make_session();
        assert!(!session.is_initialized());
    }

    #[test]
    fn session_id_successor_rejects_wraparound() {
        assert_eq!(checked_session_id_successor(1), Some(2));
        assert_eq!(checked_session_id_successor(u64::MAX - 1), Some(u64::MAX));
        assert_eq!(checked_session_id_successor(u64::MAX), None);
    }

    #[test]
    fn new_session_has_no_client_info() {
        let session = make_session();
        assert!(session.client_info().is_none());
    }

    #[test]
    fn new_session_has_no_client_capabilities() {
        let session = make_session();
        assert!(session.client_capabilities().is_none());
    }

    #[test]
    fn new_session_has_no_protocol_version() {
        let session = make_session();
        assert!(session.protocol_version().is_none());
    }

    #[test]
    fn new_session_has_no_log_level() {
        let session = make_session();
        assert!(session.log_level().is_none());
    }

    #[test]
    fn new_session_returns_server_info() {
        let session = make_session();
        assert_eq!(session.server_info().name, "test");
        assert_eq!(session.server_info().version, "1.0");
    }

    #[test]
    fn new_session_returns_server_capabilities() {
        let caps = ServerCapabilities::default();
        let session = Session::new(make_server_info(), caps);
        // default caps should be accessible
        let _ = session.server_capabilities();
    }

    // ── Initialization lifecycle ─────────────────────────────────────

    #[test]
    fn initialize_sets_initialized_flag() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        assert!(session.is_initialized());
    }

    #[test]
    fn initialize_stores_client_info() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let info = session.client_info().expect("client_info set");
        assert_eq!(info.name, "test-client");
        assert_eq!(info.version, "1.0");
    }

    #[test]
    fn initialize_stores_client_capabilities() {
        let mut session = make_session();
        let caps = ClientCapabilities {
            sampling: Some(SamplingCapability {}),
            elicitation: None,
            roots: None,
            ..Default::default()
        };
        session.initialize(make_client_info(), caps, "2024-11-05".to_string());
        let stored = session.client_capabilities().expect("caps set");
        assert!(stored.sampling.is_some());
    }

    #[test]
    fn initialize_stores_protocol_version() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2025-03-26".to_string(),
        );
        assert_eq!(session.protocol_version(), Some("2025-03-26"));
    }

    // ── Resource subscriptions ───────────────────────────────────────

    #[test]
    fn subscribe_and_check_resource() {
        let mut session = make_session();
        assert!(!session.is_resource_subscribed("file:///a.txt"));
        assert_eq!(
            admit_resource(&mut session, "file:///a.txt"),
            SubscriptionAdmission::Accepted
        );
        assert!(session.is_resource_subscribed("file:///a.txt"));
    }

    #[test]
    fn unsubscribe_resource_removes_it() {
        let mut session = make_session();
        admit_resource(&mut session, "file:///a.txt");
        assert_eq!(
            remove_resource(&mut session, "file:///a.txt"),
            SubscriptionRemoval::Removed
        );
        assert!(!session.is_resource_subscribed("file:///a.txt"));
    }

    #[test]
    fn unsubscribe_nonexistent_resource_is_noop() {
        let mut session = make_session();
        assert_eq!(
            remove_resource(&mut session, "file:///does-not-exist"),
            SubscriptionRemoval::NotSubscribed
        );
        assert!(!session.is_resource_subscribed("file:///does-not-exist"));
    }

    #[test]
    fn cancellation_during_unsubscribe_lookup_cannot_commit_removal() {
        let mut session = make_session();
        let uri = "resource://retained";
        admit_resource(&mut session, uri);
        let raw_cx = Cx::for_testing();
        let ctx = McpContext::new(raw_cx.clone(), 44);

        let result = session.unsubscribe_resource_with_precommit(&ctx, uri, || {
            raw_cx.set_cancel_requested(true);
        });

        assert_eq!(result, Err(SubscriptionRemovalError::RequestNotLive));
        assert!(session.is_resource_subscribed(uri));
        assert_eq!(session.resource_subscription_bytes, uri.len());
    }

    #[test]
    fn multiple_subscriptions_are_independent() {
        let mut session = make_session();
        admit_resource(&mut session, "a://1");
        admit_resource(&mut session, "b://2");
        assert!(session.is_resource_subscribed("a://1"));
        assert!(session.is_resource_subscribed("b://2"));
        remove_resource(&mut session, "a://1");
        assert!(!session.is_resource_subscribed("a://1"));
        assert!(session.is_resource_subscribed("b://2"));
    }

    #[test]
    fn duplicate_subscribe_is_idempotent() {
        let mut session = make_session();
        assert_eq!(
            admit_resource(&mut session, "r://x"),
            SubscriptionAdmission::Accepted
        );
        let retained_bytes = session.resource_subscription_bytes;
        assert_eq!(
            admit_resource(&mut session, "r://x"),
            SubscriptionAdmission::Duplicate
        );
        assert!(session.is_resource_subscribed("r://x"));
        assert_eq!(session.resource_subscriptions.len(), 1);
        assert_eq!(session.resource_subscription_bytes, retained_bytes);
        remove_resource(&mut session, "r://x");
        assert!(!session.is_resource_subscribed("r://x"));
        assert_eq!(session.resource_subscription_bytes, 0);
    }

    #[test]
    fn resource_subscription_count_admits_exact_cap_and_rejects_cap_plus_one() {
        let mut session = make_session();
        for index in 0..MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION {
            assert_eq!(
                admit_resource(&mut session, format!("test://subscription/{index}")),
                SubscriptionAdmission::Accepted
            );
        }

        assert_eq!(
            session.resource_subscriptions.len(),
            MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION
        );
        let retained_bytes = session.resource_subscription_bytes;
        let over_limit_uri = "test://subscription/over-limit";
        let rejection = session.subscribe_resource(&live_context(), over_limit_uri.to_string());
        assert_eq!(rejection, Err(SubscriptionAdmissionError::CapacityExceeded));
        assert!(!session.is_resource_subscribed(over_limit_uri));
        assert_eq!(session.resource_subscription_bytes, retained_bytes);

        remove_resource(&mut session, "test://subscription/0");
        assert_eq!(
            admit_resource(&mut session, over_limit_uri),
            SubscriptionAdmission::Accepted
        );
        assert!(session.is_resource_subscribed(over_limit_uri));
        assert_eq!(
            session.resource_subscriptions.len(),
            MAX_RESOURCE_SUBSCRIPTIONS_PER_SESSION
        );
    }

    #[test]
    fn resource_subscription_bytes_admit_exact_cap_reject_cap_plus_one_and_release() {
        let mut session = make_session();
        let exact_limit_uri = "é".repeat(MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION / 2);
        assert_eq!(
            exact_limit_uri.len(),
            MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION
        );

        assert_eq!(
            admit_resource(&mut session, exact_limit_uri.clone()),
            SubscriptionAdmission::Accepted
        );
        assert!(session.is_resource_subscribed(&exact_limit_uri));
        assert_eq!(
            session.resource_subscription_bytes,
            MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION
        );
        assert_eq!(
            admit_resource(&mut session, exact_limit_uri.clone()),
            SubscriptionAdmission::Duplicate
        );
        assert_eq!(
            session.resource_subscription_bytes,
            MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION
        );

        remove_resource(&mut session, &exact_limit_uri);
        assert_eq!(session.resource_subscription_bytes, 0);
        assert!(session.resource_subscriptions.is_empty());

        let over_limit_uri = format!("{exact_limit_uri}x");
        assert_eq!(
            over_limit_uri.len(),
            MAX_RESOURCE_SUBSCRIPTION_BYTES_PER_SESSION + 1
        );
        let rejection = session.subscribe_resource(&live_context(), over_limit_uri.clone());
        assert_eq!(rejection, Err(SubscriptionAdmissionError::CapacityExceeded));
        assert!(!session.is_resource_subscribed(&over_limit_uri));
        assert_eq!(session.resource_subscription_bytes, 0);
        assert_eq!(
            remove_resource(&mut session, &over_limit_uri),
            SubscriptionRemoval::NotSubscribed
        );
    }

    #[test]
    fn non_live_request_cannot_admit_or_duplicate_a_subscription() {
        let mut session = make_session();
        admit_resource(&mut session, "resource://retained");

        let cancelled_cx = Cx::for_testing();
        cancelled_cx.set_cancel_requested(true);
        let cancelled = McpContext::new(cancelled_cx, 2);

        assert_eq!(
            session.subscribe_resource(&cancelled, "resource://new".to_string()),
            Err(SubscriptionAdmissionError::RequestNotLive)
        );
        assert_eq!(
            session.subscribe_resource(&cancelled, "resource://retained".to_string()),
            Err(SubscriptionAdmissionError::RequestNotLive)
        );
        assert_eq!(
            session.unsubscribe_resource(&cancelled, "resource://retained"),
            Err(SubscriptionRemovalError::RequestNotLive)
        );
        assert!(!session.is_resource_subscribed("resource://new"));
        assert!(session.is_resource_subscribed("resource://retained"));

        let (expired, lease) = McpContext::new(Cx::for_testing(), 3)
            .begin_request_scope()
            .expect("fresh context should acquire its request lease");
        drop(lease);
        assert_eq!(
            session.subscribe_resource(&expired, "resource://expired".to_string()),
            Err(SubscriptionAdmissionError::RequestNotLive)
        );
        assert_eq!(
            session.unsubscribe_resource(&expired, "resource://retained"),
            Err(SubscriptionRemovalError::RequestNotLive)
        );
        assert!(!session.is_resource_subscribed("resource://expired"));
        assert!(session.is_resource_subscribed("resource://retained"));
    }

    // ── Log level ────────────────────────────────────────────────────

    #[test]
    fn set_log_level_and_read_back() {
        let mut session = make_session();
        session.set_log_level(LogLevel::Warning);
        assert_eq!(session.log_level(), Some(LogLevel::Warning));
    }

    #[test]
    fn set_log_level_overwrites_previous() {
        let mut session = make_session();
        session.set_log_level(LogLevel::Debug);
        session.set_log_level(LogLevel::Error);
        assert_eq!(session.log_level(), Some(LogLevel::Error));
    }

    // ── Session state ────────────────────────────────────────────────

    #[test]
    fn state_is_accessible() {
        let session = make_session();
        let state = session.state();
        // fresh state should have no stored values
        let val: Option<String> = state.get("key");
        assert!(val.is_none());
    }

    // ── notify_resource_updated ──────────────────────────────────────

    #[test]
    fn notify_resource_updated_returns_false_when_not_subscribed() {
        let session = make_session();
        let sender: NotificationSender = Arc::new(|_| {});
        assert!(!session.notify_resource_updated("file:///a.txt", &sender));
    }

    #[test]
    fn notify_resource_updated_sends_when_subscribed() {
        let mut session = make_session();
        admit_resource(&mut session, "file:///a.txt");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let sender: NotificationSender = Arc::new(move |req| {
            sent_clone.lock().unwrap().push(req);
        });

        let result = session.notify_resource_updated("file:///a.txt", &sender);
        assert!(result);

        let messages = sent.lock().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].method, "notifications/resources/updated");
    }

    #[test]
    fn notify_resource_updated_includes_uri_in_params() {
        let mut session = make_session();
        admit_resource(&mut session, "test://res");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let sender: NotificationSender = Arc::new(move |req| {
            sent_clone.lock().unwrap().push(req);
        });

        session.notify_resource_updated("test://res", &sender);

        let messages = sent.lock().unwrap();
        let params = messages[0].params.as_ref().expect("params present");
        let uri = params
            .get("uri")
            .and_then(|v| v.as_str())
            .expect("uri field");
        assert_eq!(uri, "test://res");
    }

    #[test]
    fn notify_resource_updated_does_not_fire_for_other_uri() {
        let mut session = make_session();
        admit_resource(&mut session, "file:///a.txt");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let sender: NotificationSender = Arc::new(move |req| {
            sent_clone.lock().unwrap().push(req);
        });

        let result = session.notify_resource_updated("file:///b.txt", &sender);
        assert!(!result);
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn resource_update_log_label_is_bounded_and_redacts_uri_text() {
        const URI_CANARY: &str = "private://customer-pii-canary@example.invalid/secret-token";
        let metadata = format!("{}", safe_resource_log_label(URI_CANARY));

        assert!(metadata.contains(&format!("bytes={}", URI_CANARY.len())));
        assert!(metadata.contains("sha256_prefix="));
        assert!(!metadata.contains(URI_CANARY));

        let long_uri = "x".repeat(RESOURCE_LOG_HASH_INPUT_LIMIT + 1);
        let bounded_metadata = format!("{}", safe_resource_log_label(&long_uri));
        assert!(bounded_metadata.contains(&format!(
            "hashed_prefix_bytes={RESOURCE_LOG_HASH_INPUT_LIMIT}"
        )));
        assert!(!bounded_metadata.contains(&long_uri));
    }

    #[test]
    fn notify_resource_updated_catches_sender_panic_and_returns_false() {
        let mut session = make_session();
        admit_resource(&mut session, "test://subscribed");
        let sender: NotificationSender = Arc::new(|_| {
            panic!("notification-sender-panic-payload-canary-2d941");
        });

        assert!(!session.notify_resource_updated("test://subscribed", &sender));
        assert!(session.is_resource_subscribed("test://subscribed"));
    }

    // ── Debug impl ───────────────────────────────────────────────────

    #[test]
    fn session_debug_format_includes_fields() {
        let session = make_session();
        let debug = format!("{:?}", session);
        assert!(debug.contains("Session"));
        assert!(debug.contains("initialized: false"));
    }

    #[test]
    fn session_debug_reports_only_safe_counts_and_booleans() {
        const SERVER_NAME_CANARY: &str = "server-tenant-canary-85fd";
        const SERVER_VERSION_CANARY: &str = "server-version-canary-12aa";
        const CLIENT_NAME_CANARY: &str = "customer-pii-canary@example.invalid";
        const CLIENT_VERSION_CANARY: &str = "client-version-canary-53bc";
        const PROTOCOL_CANARY: &str = "protocol-canary-secret-71f2";
        const URI_CANARY: &str = "private://subscription-uri-canary-2bd4";
        const STATE_KEY_CANARY: &str = "fastmcp.auth.canary-key-59cf";
        const STATE_VALUE_CANARY: &str = "Bearer auth-token-canary-b300";

        let mut session = Session::new(
            ServerInfo {
                name: SERVER_NAME_CANARY.to_string(),
                version: SERVER_VERSION_CANARY.to_string(),
            },
            ServerCapabilities::default(),
        );
        session.initialize(
            ClientInfo {
                name: CLIENT_NAME_CANARY.to_string(),
                version: CLIENT_VERSION_CANARY.to_string(),
            },
            ClientCapabilities::default(),
            PROTOCOL_CANARY.to_string(),
        );
        admit_resource(&mut session, URI_CANARY);
        assert!(session.state().set(STATE_KEY_CANARY, STATE_VALUE_CANARY));
        session.set_log_level(LogLevel::Debug);

        let debug = format!("{session:?}");
        assert!(debug.contains("Session"));
        assert!(debug.contains("initialized: true"));
        assert!(debug.contains("resource_subscription_count: 1"));
        assert!(debug.contains("state_entry_count: 1"));
        for canary in [
            SERVER_NAME_CANARY,
            SERVER_VERSION_CANARY,
            CLIENT_NAME_CANARY,
            CLIENT_VERSION_CANARY,
            PROTOCOL_CANARY,
            URI_CANARY,
            STATE_KEY_CANARY,
            STATE_VALUE_CANARY,
        ] {
            assert!(
                !debug.contains(canary),
                "Session Debug leaked canary: {canary}"
            );
        }
    }

    // ── Existing capability tests ────────────────────────────────────

    #[test]
    fn test_session_supports_sampling() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        // Before initialization, no capabilities
        assert!(!session.supports_sampling());

        // Initialize with sampling capability
        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
            ClientCapabilities {
                sampling: Some(SamplingCapability {}),
                elicitation: None,
                roots: None,
                ..Default::default()
            },
            "2024-11-05".to_string(),
        );

        assert!(session.supports_sampling());
        assert!(!session.supports_elicitation());
        assert!(!session.supports_roots());
    }

    #[test]
    fn test_session_supports_elicitation() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
            ClientCapabilities {
                sampling: None,
                elicitation: Some(ElicitationCapability::form()),
                roots: None,
                ..Default::default()
            },
            "2024-11-05".to_string(),
        );

        assert!(!session.supports_sampling());
        assert!(session.supports_elicitation());
        assert!(!session.supports_roots());
    }

    #[test]
    fn test_session_supports_roots() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
            ClientCapabilities {
                sampling: None,
                elicitation: None,
                roots: Some(RootsCapability { list_changed: true }),
                ..Default::default()
            },
            "2024-11-05".to_string(),
        );

        assert!(!session.supports_sampling());
        assert!(!session.supports_elicitation());
        assert!(session.supports_roots());
    }

    #[test]
    fn test_session_supports_all_capabilities() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
            ClientCapabilities {
                sampling: Some(SamplingCapability {}),
                elicitation: Some(ElicitationCapability::both()),
                roots: Some(RootsCapability {
                    list_changed: false,
                }),
                ..Default::default()
            },
            "2024-11-05".to_string(),
        );

        assert!(session.supports_sampling());
        assert!(session.supports_elicitation());
        assert!(session.supports_roots());
    }

    #[test]
    fn test_session_no_capabilities() {
        let mut session = Session::new(
            ServerInfo {
                name: "test".to_string(),
                version: "1.0".to_string(),
            },
            ServerCapabilities::default(),
        );

        session.initialize(
            ClientInfo {
                name: "test-client".to_string(),
                version: "1.0".to_string(),
            },
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );

        assert!(!session.supports_sampling());
        assert!(!session.supports_elicitation());
        assert!(!session.supports_roots());
    }

    // ── Re-initialization ───────────────────────────────────────────

    #[test]
    fn reinitialize_overwrites_client_info() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        session.initialize(
            ClientInfo {
                name: "new-client".to_string(),
                version: "2.0".to_string(),
            },
            ClientCapabilities {
                sampling: Some(SamplingCapability {}),
                elicitation: None,
                roots: None,
                ..Default::default()
            },
            "2025-03-26".to_string(),
        );
        assert!(session.is_initialized());
        let info = session.client_info().unwrap();
        assert_eq!(info.name, "new-client");
        assert_eq!(info.version, "2.0");
        assert_eq!(session.protocol_version(), Some("2025-03-26"));
        assert!(session.supports_sampling());
    }

    // ── State persistence through lifecycle ──────────────────────────

    #[test]
    fn state_persists_after_initialization() {
        let mut session = make_session();
        session.state().set("key", "before_init");
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let val: Option<String> = session.state().get("key");
        assert_eq!(val.as_deref(), Some("before_init"));
    }

    // ── Notification after unsubscribe ───────────────────────────────

    #[test]
    fn notify_resource_updated_after_unsubscribe_returns_false() {
        let mut session = make_session();
        admit_resource(&mut session, "r://x");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let sender: NotificationSender = Arc::new(move |req| {
            sent_clone.lock().unwrap().push(req);
        });

        // First notification should fire
        assert!(session.notify_resource_updated("r://x", &sender));
        assert_eq!(sent.lock().unwrap().len(), 1);

        // Unsubscribe and try again
        remove_resource(&mut session, "r://x");
        assert!(!session.notify_resource_updated("r://x", &sender));
        // No new notifications sent
        assert_eq!(sent.lock().unwrap().len(), 1);
    }

    // ── Subscribe → unsubscribe → re-subscribe ─────────────────────

    #[test]
    fn resubscribe_after_unsubscribe_works() {
        let mut session = make_session();
        admit_resource(&mut session, "r://x");
        remove_resource(&mut session, "r://x");
        assert!(!session.is_resource_subscribed("r://x"));
        admit_resource(&mut session, "r://x");
        assert!(session.is_resource_subscribed("r://x"));
    }

    // ── Debug format after initialization ───────────────────────────

    #[test]
    fn session_debug_after_init_shows_initialized_true() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        let debug = format!("{:?}", session);
        assert!(debug.contains("initialized: true"));
    }

    // ── Non-default server capabilities ─────────────────────────────

    #[test]
    fn session_with_custom_server_capabilities() {
        use fastmcp_protocol::{LoggingCapability, TasksCapability, ToolsCapability};
        let caps = ServerCapabilities {
            tools: Some(ToolsCapability { list_changed: true }),
            logging: Some(LoggingCapability {}),
            tasks: Some(TasksCapability {
                list_changed: false,
            }),
            ..ServerCapabilities::default()
        };
        let session = Session::new(make_server_info(), caps);
        assert!(session.server_capabilities().tools.is_some());
        assert!(session.server_capabilities().logging.is_some());
        assert!(session.server_capabilities().tasks.is_some());
    }

    // ── Log level with all variants ─────────────────────────────────

    #[test]
    fn set_log_level_all_variants() {
        let mut session = make_session();
        for level in [
            LogLevel::Debug,
            LogLevel::Info,
            LogLevel::Warning,
            LogLevel::Error,
        ] {
            session.set_log_level(level);
            assert_eq!(session.log_level(), Some(level));
        }
    }

    // ── Persistence across re-initialization ────────────────────────

    #[test]
    fn log_level_persists_across_reinitialization() {
        let mut session = make_session();
        session.set_log_level(LogLevel::Warning);
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        assert_eq!(session.log_level(), Some(LogLevel::Warning));
        // Re-initialize with different client info
        session.initialize(
            ClientInfo {
                name: "other".to_string(),
                version: "2.0".to_string(),
            },
            ClientCapabilities::default(),
            "2025-03-26".to_string(),
        );
        assert_eq!(session.log_level(), Some(LogLevel::Warning));
    }

    #[test]
    fn resource_subscriptions_persist_across_reinitialization() {
        let mut session = make_session();
        admit_resource(&mut session, "file:///keep.txt");
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        assert!(session.is_resource_subscribed("file:///keep.txt"));
    }

    #[test]
    fn state_set_after_init_persists_through_reinit() {
        let mut session = make_session();
        session.initialize(
            make_client_info(),
            ClientCapabilities::default(),
            "2024-11-05".to_string(),
        );
        session.state().set("counter", 42);
        // Re-initialize
        session.initialize(
            ClientInfo {
                name: "new".to_string(),
                version: "3.0".to_string(),
            },
            ClientCapabilities::default(),
            "2025-03-26".to_string(),
        );
        let val: Option<i32> = session.state().get("counter");
        assert_eq!(val, Some(42));
    }

    #[test]
    fn notify_resource_updated_fires_independently_per_subscription() {
        let mut session = make_session();
        admit_resource(&mut session, "a://1");
        admit_resource(&mut session, "b://2");

        let sent = Arc::new(Mutex::new(Vec::new()));
        let sent_clone = Arc::clone(&sent);
        let sender: NotificationSender = Arc::new(move |req| {
            sent_clone.lock().unwrap().push(req);
        });

        // Notify first URI
        assert!(session.notify_resource_updated("a://1", &sender));
        assert_eq!(sent.lock().unwrap().len(), 1);
        let uri = sent.lock().unwrap()[0]
            .params
            .as_ref()
            .unwrap()
            .get("uri")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(uri, "a://1");

        // Notify second URI
        assert!(session.notify_resource_updated("b://2", &sender));
        assert_eq!(sent.lock().unwrap().len(), 2);
        let uri2 = sent.lock().unwrap()[1]
            .params
            .as_ref()
            .unwrap()
            .get("uri")
            .unwrap()
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(uri2, "b://2");
    }

    #[test]
    fn supports_elicitation_and_roots_false_before_init() {
        let session = make_session();
        assert!(!session.supports_elicitation());
        assert!(!session.supports_roots());
    }

    #[test]
    fn principal_binding_is_write_once_and_shared_across_clones() {
        let session = make_session();
        let binding = session.principal_binding();
        let clone = binding.clone();
        let alice = Sha256Digest::from_bytes([0x11; 32]);
        let bob = Sha256Digest::from_bytes([0x22; 32]);

        assert!(!binding.is_bound_for_debug());
        assert!(!binding.verify_existing(alice));
        assert!(binding.bind_or_verify(alice));
        assert!(binding.verify_existing(alice));
        assert!(!binding.verify_existing(bob));
        assert!(clone.bind_or_verify(alice));
        assert!(!clone.bind_or_verify(bob));
        assert!(binding.bind_or_verify(alice));
    }

    #[test]
    fn session_debug_reports_only_principal_presence() {
        let session = make_session();
        let canary = Sha256Digest::from_bytes([0xAB; 32]);
        assert!(session.principal_binding().bind_or_verify(canary));

        let debug = format!("{session:?}");
        assert!(debug.contains("principal_bound: true"));
        assert!(!debug.contains("abab"));
    }
}
