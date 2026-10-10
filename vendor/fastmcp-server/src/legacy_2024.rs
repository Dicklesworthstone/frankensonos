//! Exact MCP 2024-11-05 server lifecycle adapter.
//!
//! This module is transport-neutral: a caller supplies one preselected peer
//! binding and complete JSON values, then carries the returned JSON values over
//! its own framing.  It does not parse HTTP/SSE/stdio frames, choose a route, or
//! perform authentication and authorization.  Those concerns are explicit
//! integration responsibilities.

mod admission;

use std::collections::BTreeSet;

use admission::PreparedReceive;
use fastmcp_protocol::JsonInteger;
use fastmcp_protocol::methods::Legacy2024EnvelopeError;
use fastmcp_protocol::methods::{
    COMPLETION_COMPLETE, INITIALIZE, LEGACY_2024_11_05_PROTOCOL_VERSION, LOGGING_SET_LEVEL,
    Legacy2024Capability, Legacy2024ClientCapabilities, Legacy2024Direction, Legacy2024Envelope,
    Legacy2024ServerCapabilities, NOTIFICATIONS_CANCELLED, NOTIFICATIONS_INITIALIZED,
    NOTIFICATIONS_MESSAGE, NOTIFICATIONS_PROGRESS, NOTIFICATIONS_PROMPTS_LIST_CHANGED,
    NOTIFICATIONS_RESOURCES_LIST_CHANGED, NOTIFICATIONS_RESOURCES_UPDATED,
    NOTIFICATIONS_ROOTS_LIST_CHANGED, NOTIFICATIONS_TOOLS_LIST_CHANGED, PING, PROMPTS_GET,
    PROMPTS_LIST, RESOURCES_LIST, RESOURCES_READ, RESOURCES_SUBSCRIBE, RESOURCES_TEMPLATES_LIST,
    RESOURCES_UNSUBSCRIBE, ROOTS_LIST, SAMPLING_CREATE_MESSAGE, TOOLS_CALL, TOOLS_LIST,
    decode_legacy_2024_11_05_client_capabilities, decode_legacy_2024_11_05_envelope_classified,
    translate_legacy_2024_result, validate_legacy_2024_11_05_initialize_result,
    validate_legacy_2024_11_05_method_params,
};
use serde_json::{Value, json};

fn validate_application_tool_result(result: Value) -> Result<Value, Legacy2024AdapterError> {
    let invalid = || Legacy2024AdapterError {
        code: JsonInteger::from(-32603),
        message: "handler result is not valid application tool content on a legacy connection"
            .to_owned(),
    };
    let object = result.as_object().ok_or_else(invalid)?;
    let content = object.get("content").ok_or_else(invalid)?;
    serde_json::from_value::<Vec<fastmcp_protocol::Content>>(content.clone())
        .map_err(|_| invalid())?;

    // Reuse exact validation for every envelope member, including reserved
    // metadata and modern-only fields. Only content uses the application's
    // existing typed vocabulary. This validation projection is never emitted;
    // the original complete result crosses the normal response boundary.
    let mut envelope: serde_json::Map<String, Value> = object
        .iter()
        .filter(|(key, _)| key.as_str() != "content")
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    envelope.insert("content".to_owned(), Value::Array(Vec::new()));
    translate_legacy_2024_result(TOOLS_CALL, Value::Object(envelope)).map_err(|_| invalid())?;
    Ok(result)
}

/// Maximum combined subscriptions and pending reverse requests retained by
/// one exact-2024 adapter binding.
///
/// The adapter shares the server's bounded bidirectional-request ceiling so a
/// legacy connection cannot accumulate an independent unbounded state set.
pub const LEGACY_2024_MAX_ADAPTER_RESERVATIONS: usize =
    crate::bidirectional::DEFAULT_MAX_IN_FLIGHT_REQUESTS;

/// Opaque, authenticated transport-owner partition for legacy peer state.
///
/// The transport creates this value only after it has authenticated its peer.
/// This adapter deliberately does not inspect or authorize those bytes: it
/// binds every lifecycle object and installation receipt to the supplied
/// partition so a generation is never sufficient to select another peer's
/// state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LegacyAuthenticatedPeerPartition {
    bytes: [u8; 32],
}

impl LegacyAuthenticatedPeerPartition {
    /// Fixed size of the opaque transport-authenticated partition identifier.
    pub const BYTE_LEN: usize = 32;

    /// Wraps the transport-authenticated owner partition without interpreting
    /// the authentication mechanism or its credentials.
    #[must_use]
    pub const fn from_authenticated_transport(bytes: [u8; Self::BYTE_LEN]) -> Self {
        Self { bytes }
    }

    fn bytes(self) -> [u8; Self::BYTE_LEN] {
        self.bytes
    }
}

/// Opaque transport-supplied identity for one exact-2024 peer lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LegacyPeerBinding {
    owner_partition: LegacyAuthenticatedPeerPartition,
    generation: u64,
}

impl LegacyPeerBinding {
    /// Binds a transport-authenticated owner partition to a monotonically
    /// unique transport generation.
    ///
    /// A generation alone cannot create a binding or select adapter state.
    #[must_use]
    pub const fn from_authenticated_transport(
        owner_partition: LegacyAuthenticatedPeerPartition,
        generation: u64,
    ) -> Self {
        Self {
            owner_partition,
            generation,
        }
    }

    /// Returns the opaque generation for receipt correlation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }

    fn canonical_bytes(self) -> [u8; LegacyAuthenticatedPeerPartition::BYTE_LEN + 8] {
        let mut bytes = [0; LegacyAuthenticatedPeerPartition::BYTE_LEN + 8];
        bytes[..LegacyAuthenticatedPeerPartition::BYTE_LEN]
            .copy_from_slice(&self.owner_partition.bytes());
        bytes[LegacyAuthenticatedPeerPartition::BYTE_LEN..]
            .copy_from_slice(&self.generation.to_be_bytes());
        bytes
    }
}

/// One binding's exact lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Legacy2024Lifecycle {
    /// An exact initialize request has not yet been accepted.
    AwaitInitialize,
    /// Initialize response committed; the initialized notification is required.
    AwaitInitialized,
    /// Exact 2024 operating messages may now be handled.
    Operating,
    /// Terminal state. No later wire message may mutate adapter state.
    Closed,
}

/// Server metadata emitted in a successful exact-2024 initialize result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy2024ServerInfo {
    /// Server implementation name.
    pub name: String,
    /// Server implementation version.
    pub version: String,
}

/// Frozen server values supplied when constructing one adapter binding.
#[derive(Debug, Clone, PartialEq)]
pub struct Legacy2024ServerConfig {
    /// Exact legacy server capabilities.
    pub capabilities: Legacy2024ServerCapabilities,
    /// Server implementation metadata.
    pub server_info: Legacy2024ServerInfo,
    /// Optional exact-2024 server instructions.
    pub instructions: Option<String>,
}

/// Opaque proof that this adapter was installed for one exact legacy binding.
///
/// Its fields intentionally remain private: consumers may retain or compare
/// the receipt, but cannot mint one from protocol facts or a boolean claim.
#[derive(Debug, PartialEq, Eq)]
pub struct LegacyServerAdapterInstalledReceipt {
    binding: LegacyPeerBinding,
    protocol_version: &'static str,
}

impl LegacyServerAdapterInstalledReceipt {
    /// Returns the exact protocol era bound by the real installation.
    #[must_use]
    pub const fn protocol_version(&self) -> &'static str {
        self.protocol_version
    }

    /// Returns canonical, receipt-bound bytes for a downstream verifier.
    ///
    /// This is an observation surface only; there is no public constructor for
    /// a receipt from these bytes or their constituent facts.
    #[must_use]
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let mut bytes = b"fastmcp-legacy-server-install-v2\0".to_vec();
        for field in [
            self.protocol_version.as_bytes(),
            self.binding.canonical_bytes().as_slice(),
        ] {
            bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
            bytes.extend_from_slice(field);
        }
        bytes
    }

    /// Returns whether this sealed receipt was emitted by the supplied
    /// authenticated transport binding.
    #[must_use]
    pub fn matches_binding(&self, binding: LegacyPeerBinding) -> bool {
        self.binding == binding
    }
}

/// A transport-neutral operation delegated after lifecycle and capability admission.
pub trait Legacy2024Handler {
    /// Handles one admitted client-to-server request and returns its exact result object.
    fn handle_legacy_2024(
        &mut self,
        method: &'static str,
        params: Option<&Value>,
    ) -> Result<Value, Legacy2024HandlerError>;

    /// Handles one admitted request with its original exact JSON-RPC ID.
    ///
    /// The default preserves the transport-neutral handler contract for
    /// adapters that do not need request-local correlation. Live runtimes
    /// override this hook so cancellation, tracing, and dispatch authority
    /// remain bound to the peer's actual wire request rather than a synthetic
    /// internal identifier.
    fn handle_legacy_2024_with_request_id(
        &mut self,
        _request_id: &Value,
        method: &'static str,
        params: Option<&Value>,
    ) -> Result<Value, Legacy2024HandlerError> {
        self.handle_legacy_2024(method, params)
    }

    /// Request-owned async dispatch for one admitted exact-2024 request.
    ///
    /// The default preserves the transport-neutral sync handler contract.
    /// Live HTTP/stdio runtimes override this so handler futures are polled
    /// on the request `Cx` instead of `block_on`.
    fn handle_legacy_2024_with_request_id_async<'a>(
        &'a mut self,
        request_id: &'a Value,
        method: &'static str,
        params: Option<&'a Value>,
    ) -> crate::BoxFuture<'a, Result<Value, Legacy2024HandlerError>> {
        // Evaluate the sync contract immediately so the returned future does
        // not capture `&mut Self`. Many adapter tests use `!Send` handlers.
        // BoxFuture requires Send; `&mut Self` is not Send when Self is not.
        let result = self.handle_legacy_2024_with_request_id(request_id, method, params);
        Box::pin(async move { result })
    }
}

/// A non-wire handler failure mapped to an exact JSON-RPC internal-error response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy2024HandlerError {
    code: JsonInteger,
    message: String,
}

impl Legacy2024HandlerError {
    /// Creates a bounded handler failure message for the adapter's error mapping.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            code: JsonInteger::from(-32603),
            message: message.into(),
        }
    }

    /// Creates a handler failure with a JSON-RPC error code already selected
    /// by the live request owner.
    #[must_use]
    pub fn with_code(code: impl Into<JsonInteger>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }

    /// Returns the JSON-RPC error code selected by the handler.
    #[must_use]
    pub fn code(&self) -> &JsonInteger {
        &self.code
    }

    /// Returns the handler-provided diagnostic retained on the exact-2024 wire.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

/// Transport-neutral outbound item from the exact-2024 adapter.
#[derive(Debug, Clone, PartialEq)]
pub enum Legacy2024Outbound {
    /// A JSON-RPC response to an inbound request.
    Response(Value),
    /// A server-to-client JSON-RPC request constructed from negotiated capabilities.
    ReverseRequest(Value),
    /// A server-to-client JSON-RPC notification constructed from advertised capabilities.
    ReverseNotification(Value),
    /// A valid notification produces no JSON-RPC response.
    NoResponse,
}

/// Exact lifecycle and admission failure which cannot safely emit a response.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy2024AdapterError {
    code: JsonInteger,
    message: String,
}

impl Legacy2024AdapterError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: JsonInteger::from(-32600),
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: JsonInteger::from(-32602),
            message: message.into(),
        }
    }

    fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: JsonInteger::from(-32601),
            message: message.into(),
        }
    }

    /// Exact JSON-RPC error code.
    #[must_use]
    pub fn code(&self) -> &JsonInteger {
        &self.code
    }

    /// Refusal message retained from the adapter or the live handler.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Legacy2024AdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Legacy2024AdapterError {}

/// Compact immutable view for mutation-free planted-negative assertions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy2024StateSnapshot {
    /// Current lifecycle state.
    pub lifecycle: Legacy2024Lifecycle,
    /// Number of successful transitions into `Operating` for this binding.
    pub operating_transition_count: u64,
    /// Frozen exact client capability bytes, if initialization committed.
    pub client_capabilities_bytes: Vec<u8>,
    /// Exact retained resource subscriptions in stable order.
    pub subscriptions: Vec<String>,
    /// Current client logging level, if one was admitted.
    pub logging_level: Option<String>,
    /// Exact pending server-to-client request IDs in stable order.
    pub pending_reverse_request_ids: Vec<i64>,
    /// Number of live adapter-owned quota reservations.
    pub reservation_count: u64,
    /// Count of admitted cancellation/progress control notifications.
    pub control_notification_count: u64,
    /// Count of accepted client root-list-change notifications.
    pub roots_list_changed_count: u64,
    /// Count of completed terminal close releases.
    pub close_release_count: u64,
    /// Next reverse request ID, before any later allocation.
    pub next_reverse_request_id: i64,
}

impl Legacy2024StateSnapshot {
    /// Returns the canonical byte representation used as a LEG-02 row's
    /// adapter-state digest. This contains every mutable adapter-owned field
    /// represented by this snapshot, in a stable order.
    #[must_use]
    pub fn canonical_digest(&self) -> Vec<u8> {
        let mut bytes = b"fastmcp-legacy-2024-state-v1\0".to_vec();
        append_length_prefixed(&mut bytes, lifecycle_bytes(self.lifecycle));
        append_length_prefixed(&mut bytes, &self.operating_transition_count.to_be_bytes());
        append_length_prefixed(&mut bytes, &self.client_capabilities_bytes);
        append_length_prefixed(&mut bytes, &(self.subscriptions.len() as u32).to_be_bytes());
        for subscription in &self.subscriptions {
            append_length_prefixed(&mut bytes, subscription.as_bytes());
        }
        append_length_prefixed(&mut bytes, &[u8::from(self.logging_level.is_some())]);
        append_length_prefixed(
            &mut bytes,
            self.logging_level.as_deref().unwrap_or_default().as_bytes(),
        );
        append_length_prefixed(
            &mut bytes,
            &(self.pending_reverse_request_ids.len() as u32).to_be_bytes(),
        );
        for request_id in &self.pending_reverse_request_ids {
            append_length_prefixed(&mut bytes, &request_id.to_be_bytes());
        }
        append_length_prefixed(&mut bytes, &self.reservation_count.to_be_bytes());
        append_length_prefixed(&mut bytes, &self.control_notification_count.to_be_bytes());
        append_length_prefixed(&mut bytes, &self.roots_list_changed_count.to_be_bytes());
        append_length_prefixed(&mut bytes, &self.close_release_count.to_be_bytes());
        append_length_prefixed(&mut bytes, &self.next_reverse_request_id.to_be_bytes());
        bytes
    }
}

/// Exact MCP 2024-11-05 server adapter for one transport-selected binding.
pub struct Legacy2024ServerAdapter<H> {
    binding: LegacyPeerBinding,
    installed_receipt: LegacyServerAdapterInstalledReceipt,
    lifecycle: Legacy2024Lifecycle,
    operating_transition_count: u64,
    config: Legacy2024ServerConfig,
    handler: H,
    application_tool_content: bool,
    client_capabilities: Option<Legacy2024ClientCapabilities>,
    client_capabilities_bytes: Vec<u8>,
    client_info: Option<fastmcp_protocol::ClientInfo>,
    subscriptions: BTreeSet<String>,
    logging_level: Option<String>,
    pending_reverse_request_ids: BTreeSet<i64>,
    reservation_count: u64,
    control_notification_count: u64,
    roots_list_changed_count: u64,
    close_release_count: u64,
    next_reverse_request_id: i64,
}

struct LiveLegacy2024Binding<H> {
    binding: LegacyPeerBinding,
    adapter: Legacy2024ServerAdapter<H>,
}

/// Bounded reusable host for exact-2024 peer lifecycles in one live server.
///
/// A transport consumer supplies an already authenticated, fresh
/// [`LegacyPeerBinding`] for each connection or process generation, then uses
/// this host for that binding's complete inbound, outbound, and close
/// lifecycle. The host does not parse transport frames, select a protocol era,
/// or manufacture bindings. Those responsibilities remain with its stdio or
/// HTTP consumer.
///
/// Closing a binding releases its adapter-owned state and removes it from this
/// live host. Callers must not reuse a closed binding: a transport generation
/// is monotonically unique by contract, so a reconnect must supply a fresh
/// [`LegacyPeerBinding`].
pub struct Legacy2024LiveServerLifecycle<H, Factory> {
    config: Legacy2024ServerConfig,
    max_live_bindings: usize,
    make_handler: Factory,
    bindings: Vec<LiveLegacy2024Binding<H>>,
}

impl<H, Factory> Legacy2024LiveServerLifecycle<H, Factory>
where
    H: Legacy2024Handler,
    Factory: FnMut(LegacyPeerBinding) -> H,
{
    /// Creates an empty, bounded host with frozen exact-2024 server values.
    ///
    /// The handler factory is called only after the live-binding capacity has
    /// been reserved, and exactly once for each successfully installed peer.
    pub fn new(
        config: Legacy2024ServerConfig,
        max_live_bindings: usize,
        make_handler: Factory,
    ) -> Result<Self, Legacy2024AdapterError> {
        if max_live_bindings == 0 {
            return Err(Legacy2024AdapterError::invalid_params(
                "legacy live lifecycle requires a nonzero binding limit",
            ));
        }
        initialize_result(&config)?;
        Ok(Self {
            config,
            max_live_bindings,
            make_handler,
            bindings: Vec::new(),
        })
    }

    /// Installs one fresh exact-2024 lifecycle for a transport-selected peer.
    ///
    /// No handler is created and no live state changes when the binding is
    /// already active, the fixed live-binding limit is reached, or storage
    /// cannot be reserved.
    pub fn install(
        &mut self,
        binding: LegacyPeerBinding,
    ) -> Result<&LegacyServerAdapterInstalledReceipt, Legacy2024AdapterError> {
        if self.is_binding_live(binding) {
            return Err(Legacy2024AdapterError::invalid_request(
                "legacy peer binding already owns a live adapter lifecycle",
            ));
        }
        if self.bindings.len() >= self.max_live_bindings {
            return Err(Legacy2024AdapterError::invalid_request(
                "legacy live lifecycle binding limit reached",
            ));
        }
        self.bindings.try_reserve(1).map_err(|_| {
            Legacy2024AdapterError::invalid_request(
                "legacy live lifecycle cannot reserve binding state",
            )
        })?;

        let adapter = Legacy2024ServerAdapter::install(
            binding,
            self.config.clone(),
            (self.make_handler)(binding),
        )?;
        let index = self.bindings.len();
        self.bindings
            .push(LiveLegacy2024Binding { binding, adapter });
        Ok(self.bindings[index].adapter.installed_receipt())
    }

    /// Returns the number of currently live peer lifecycles.
    #[must_use]
    pub const fn active_binding_count(&self) -> usize {
        self.bindings.len()
    }

    /// Returns the immutable maximum number of concurrent peer lifecycles.
    #[must_use]
    pub const fn max_live_bindings(&self) -> usize {
        self.max_live_bindings
    }

    /// Returns whether a binding currently owns one live adapter lifecycle.
    #[must_use]
    pub fn is_binding_live(&self, binding: LegacyPeerBinding) -> bool {
        self.bindings
            .iter()
            .any(|candidate| candidate.binding == binding)
    }

    /// Returns the immutable state view for one live peer lifecycle.
    pub fn snapshot(
        &self,
        binding: LegacyPeerBinding,
    ) -> Result<Legacy2024StateSnapshot, Legacy2024AdapterError> {
        Ok(self.adapter(binding)?.snapshot())
    }

    /// Applies one complete client-to-server JSON value to its live adapter.
    pub fn receive(
        &mut self,
        binding: LegacyPeerBinding,
        wire: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.adapter_mut(binding)?.receive(binding, wire)
    }

    /// Applies one complete client-to-server JSON value without `block_on`.
    pub async fn receive_async(
        &mut self,
        binding: LegacyPeerBinding,
        wire: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.adapter_mut(binding)?
            .receive_async(binding, wire)
            .await
    }

    /// Builds one capability-gated server-to-client request for a live peer.
    pub fn make_reverse_request(
        &mut self,
        binding: LegacyPeerBinding,
        method: &'static str,
        params: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.adapter_mut(binding)?
            .make_reverse_request(binding, method, params)
    }

    /// Builds one capability-gated server-to-client notification for a live peer.
    pub fn make_notification(
        &self,
        binding: LegacyPeerBinding,
        method: &'static str,
        params: Option<Value>,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.adapter(binding)?
            .make_notification(binding, method, params)
    }

    /// Closes one live binding, releases its owned state, and removes it.
    ///
    /// The returned snapshot is the terminal observation: its lifecycle is
    /// `Closed`, its reservation count is zero, and its release count has
    /// advanced exactly once. A failed close leaves the live binding intact.
    pub fn close(
        &mut self,
        binding: LegacyPeerBinding,
    ) -> Result<Legacy2024StateSnapshot, Legacy2024AdapterError> {
        let index = self.binding_index(binding)?;
        let terminal_snapshot = {
            let adapter = &mut self.bindings[index].adapter;
            adapter.close(binding)?;
            adapter.snapshot()
        };
        self.bindings.swap_remove(index);
        Ok(terminal_snapshot)
    }

    fn adapter(
        &self,
        binding: LegacyPeerBinding,
    ) -> Result<&Legacy2024ServerAdapter<H>, Legacy2024AdapterError> {
        let index = self.binding_index(binding)?;
        Ok(&self.bindings[index].adapter)
    }

    fn adapter_mut(
        &mut self,
        binding: LegacyPeerBinding,
    ) -> Result<&mut Legacy2024ServerAdapter<H>, Legacy2024AdapterError> {
        let index = self.binding_index(binding)?;
        Ok(&mut self.bindings[index].adapter)
    }

    fn binding_index(&self, binding: LegacyPeerBinding) -> Result<usize, Legacy2024AdapterError> {
        self.bindings
            .iter()
            .position(|candidate| candidate.binding == binding)
            .ok_or(Legacy2024AdapterError::invalid_request(
                "legacy peer binding has no live adapter lifecycle",
            ))
    }
}

impl<H> Legacy2024ServerAdapter<H>
where
    H: Legacy2024Handler,
{
    /// Installs an uninitialized adapter for exactly one selected peer binding.
    ///
    /// Installation validates the eventual exact initialize result before an
    /// adapter or its opaque receipt can exist.
    pub fn install(
        binding: LegacyPeerBinding,
        config: Legacy2024ServerConfig,
        handler: H,
    ) -> Result<Self, Legacy2024AdapterError> {
        initialize_result(&config)?;
        Ok(Self {
            binding,
            installed_receipt: LegacyServerAdapterInstalledReceipt {
                binding,
                protocol_version: LEGACY_2024_11_05_PROTOCOL_VERSION,
            },
            lifecycle: Legacy2024Lifecycle::AwaitInitialize,
            operating_transition_count: 0,
            config,
            handler,
            application_tool_content: false,
            client_capabilities: None,
            client_capabilities_bytes: Vec::new(),
            client_info: None,
            subscriptions: BTreeSet::new(),
            logging_level: None,
            pending_reverse_request_ids: BTreeSet::new(),
            reservation_count: 0,
            control_notification_count: 0,
            roots_list_changed_count: 0,
            close_release_count: 0,
            next_reverse_request_id: 1,
        })
    }

    /// Selects only the application-owned tool content projection.
    pub(crate) fn with_application_tool_content(mut self, enabled: bool) -> Self {
        self.application_tool_content = enabled;
        self
    }

    /// Returns the binding that exclusively owns this lifecycle state.
    #[must_use]
    pub const fn binding(&self) -> LegacyPeerBinding {
        self.binding
    }

    /// Returns the opaque receipt produced by this real installation.
    #[must_use]
    pub const fn installed_receipt(&self) -> &LegacyServerAdapterInstalledReceipt {
        &self.installed_receipt
    }

    /// Returns the current lifecycle state.
    #[must_use]
    pub const fn lifecycle(&self) -> Legacy2024Lifecycle {
        self.lifecycle
    }

    /// Returns the initialize `clientInfo` retained after a successful handshake.
    #[must_use]
    pub fn client_info(&self) -> Option<&fastmcp_protocol::ClientInfo> {
        self.client_info.as_ref()
    }

    /// Returns a deterministic state snapshot without exposing handler internals.
    #[must_use]
    pub fn snapshot(&self) -> Legacy2024StateSnapshot {
        Legacy2024StateSnapshot {
            lifecycle: self.lifecycle,
            operating_transition_count: self.operating_transition_count,
            client_capabilities_bytes: self.client_capabilities_bytes.clone(),
            subscriptions: self.subscriptions.iter().cloned().collect(),
            logging_level: self.logging_level.clone(),
            pending_reverse_request_ids: self.pending_reverse_request_ids.iter().copied().collect(),
            reservation_count: self.reservation_count,
            control_notification_count: self.control_notification_count,
            roots_list_changed_count: self.roots_list_changed_count,
            close_release_count: self.close_release_count,
            next_reverse_request_id: self.next_reverse_request_id,
        }
    }

    /// Applies one inbound client-to-server JSON value for the selected binding.
    ///
    /// Raw exact-2024 admission happens before any lifecycle or handler state
    /// mutation. Request failures become exact JSON-RPC errors; notification
    /// failures are returned because notifications cannot have responses.
    /// This entry point invokes the synchronous request-ID hook directly. A
    /// handler implemented through async overrides must use `receive_async`.
    /// The adapter never constructs a handler future or enters a runtime here.
    pub fn receive(
        &mut self,
        binding: LegacyPeerBinding,
        wire: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        match self.prepare_receive(binding, wire)? {
            PreparedReceive::Outbound(outbound) => Ok(outbound),
            PreparedReceive::Dispatch { id, method, params } => {
                let result = self.handler.handle_legacy_2024_with_request_id(
                    &id,
                    method,
                    params.as_ref(),
                );
                Ok(self.finish_receive(id, method, params.as_ref(), result))
            }
        }
    }

    /// Applies one inbound value by awaiting the admitted handler on the caller.
    /// Shares wire, lifecycle, capability and result admission with `receive`;
    /// only selection of the application execution hook differs.
    pub async fn receive_async(
        &mut self,
        binding: LegacyPeerBinding,
        wire: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        match self.prepare_receive(binding, wire)? {
            PreparedReceive::Outbound(outbound) => Ok(outbound),
            PreparedReceive::Dispatch { id, method, params } => {
                let result = self.handler.handle_legacy_2024_with_request_id_async(
                    &id,
                    method,
                    params.as_ref(),
                ).await;
                Ok(self.finish_receive(id, method, params.as_ref(), result))
            }
        }
    }

    /// Builds a capability-gated server-to-client request without transport framing.
    pub fn make_reverse_request(
        &mut self,
        binding: LegacyPeerBinding,
        method: &'static str,
        params: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.require_binding(binding)?;
        if self.lifecycle != Legacy2024Lifecycle::Operating {
            return Err(Legacy2024AdapterError::invalid_request(
                "reverse requests require Operating lifecycle",
            ));
        }
        if !params.is_object() {
            return Err(Legacy2024AdapterError::invalid_params(
                "reverse request params must be an object",
            ));
        }
        validate_legacy_2024_11_05_method_params(method, Some(&params)).map_err(|_| {
            Legacy2024AdapterError::invalid_params(
                "reverse request params are not exact MCP 2024-11-05",
            )
        })?;
        let capability = match method {
            ROOTS_LIST => Legacy2024Capability::ClientRoots,
            SAMPLING_CREATE_MESSAGE => Legacy2024Capability::ClientSampling,
            PING => return self.emit_reverse_request(method, params),
            _ => {
                return Err(Legacy2024AdapterError::method_not_found(
                    "method is not an exact MCP 2024-11-05 server-to-client request",
                ));
            }
        };
        if !self.client_supports(capability) {
            return Err(Legacy2024AdapterError::invalid_request(
                "negotiated client capabilities do not permit reverse request",
            ));
        }
        self.emit_reverse_request(method, params)
    }

    /// Builds a capability-gated exact server-to-client notification without
    /// framing it for a transport.
    pub fn make_notification(
        &self,
        binding: LegacyPeerBinding,
        method: &'static str,
        params: Option<Value>,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        self.require_binding(binding)?;
        if self.lifecycle != Legacy2024Lifecycle::Operating {
            return Err(Legacy2024AdapterError::invalid_request(
                "reverse notifications require Operating lifecycle",
            ));
        }
        validate_legacy_2024_11_05_method_params(method, params.as_ref()).map_err(|_| {
            Legacy2024AdapterError::invalid_params(
                "notification params are not exact MCP 2024-11-05",
            )
        })?;
        let capability_permitted = match method {
            NOTIFICATIONS_CANCELLED | NOTIFICATIONS_PROGRESS => true,
            NOTIFICATIONS_MESSAGE => self.config.capabilities.logging.is_some(),
            NOTIFICATIONS_PROMPTS_LIST_CHANGED => self
                .config
                .capabilities
                .prompts
                .as_ref()
                .is_some_and(|prompts| prompts.list_changed),
            NOTIFICATIONS_RESOURCES_LIST_CHANGED => self
                .config
                .capabilities
                .resources
                .as_ref()
                .is_some_and(|resources| resources.list_changed),
            NOTIFICATIONS_RESOURCES_UPDATED => params
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|params| params.get("uri"))
                .and_then(Value::as_str)
                .is_some_and(|uri| {
                    self.config
                        .capabilities
                        .resources
                        .as_ref()
                        .is_some_and(|resources| resources.subscribe)
                        && self.subscriptions.contains(uri)
                }),
            NOTIFICATIONS_TOOLS_LIST_CHANGED => self
                .config
                .capabilities
                .tools
                .as_ref()
                .is_some_and(|tools| tools.list_changed),
            _ => {
                return Err(Legacy2024AdapterError::method_not_found(
                    "method is not an exact MCP 2024-11-05 server-to-client notification",
                ));
            }
        };
        if !capability_permitted {
            return Err(Legacy2024AdapterError::invalid_request(
                "advertised exact MCP 2024-11-05 capability does not permit notification",
            ));
        }
        let mut notification = json!({"jsonrpc": "2.0", "method": method});
        if let Some(params) = params {
            notification["params"] = params;
        }
        Ok(Legacy2024Outbound::ReverseNotification(notification))
    }

    /// Transitions to terminal Closed and releases all adapter-owned state once.
    pub fn close(&mut self, binding: LegacyPeerBinding) -> Result<(), Legacy2024AdapterError> {
        self.require_binding(binding)?;
        if self.lifecycle == Legacy2024Lifecycle::Closed {
            return Err(Legacy2024AdapterError::invalid_request(
                "legacy adapter lifecycle is already closed",
            ));
        }
        self.lifecycle = Legacy2024Lifecycle::Closed;
        self.client_capabilities = None;
        self.client_capabilities_bytes.clear();
        self.subscriptions.clear();
        self.logging_level = None;
        self.pending_reverse_request_ids.clear();
        self.reservation_count = 0;
        self.close_release_count = self.close_release_count.saturating_add(1);
        Ok(())
    }

    fn require_binding(&self, binding: LegacyPeerBinding) -> Result<(), Legacy2024AdapterError> {
        if self.binding == binding {
            Ok(())
        } else {
            Err(Legacy2024AdapterError::invalid_request(
                "legacy peer binding does not own this adapter lifecycle",
            ))
        }
    }

    fn receive_notification(
        &mut self,
        method: &'static str,
        params: Option<&Value>,
    ) -> Result<(), Legacy2024AdapterError> {
        match self.lifecycle {
            Legacy2024Lifecycle::AwaitInitialize => Err(Legacy2024AdapterError::invalid_request(
                "initialize is required before notifications/initialized",
            )),
            Legacy2024Lifecycle::AwaitInitialized if method == NOTIFICATIONS_INITIALIZED => {
                self.lifecycle = Legacy2024Lifecycle::Operating;
                self.operating_transition_count = self.operating_transition_count.saturating_add(1);
                Ok(())
            }
            Legacy2024Lifecycle::AwaitInitialized => Err(Legacy2024AdapterError::invalid_request(
                "only notifications/initialized is allowed after initialize response",
            )),
            Legacy2024Lifecycle::Operating => match method {
                NOTIFICATIONS_CANCELLED | NOTIFICATIONS_PROGRESS => {
                    if params.is_none() {
                        return Err(Legacy2024AdapterError::invalid_params(
                            "cancellation and progress notifications require params",
                        ));
                    }
                    self.control_notification_count =
                        self.control_notification_count.saturating_add(1);
                    Ok(())
                }
                NOTIFICATIONS_ROOTS_LIST_CHANGED => {
                    if !self.client_supports(Legacy2024Capability::ClientRootsListChanged) {
                        return Err(Legacy2024AdapterError::invalid_request(
                            "client roots.listChanged capability is required",
                        ));
                    }
                    self.roots_list_changed_count = self.roots_list_changed_count.saturating_add(1);
                    Ok(())
                }
                NOTIFICATIONS_INITIALIZED => Err(Legacy2024AdapterError::invalid_request(
                    "notifications/initialized may be sent exactly once",
                )),
                _ => Err(Legacy2024AdapterError::method_not_found(
                    "notification direction or method is not admitted by exact MCP 2024-11-05",
                )),
            },
            Legacy2024Lifecycle::Closed => Err(Legacy2024AdapterError::invalid_request(
                "legacy adapter lifecycle is closed",
            )),
        }
    }

    fn admit_initialize(
        &mut self,
        params: Option<&Value>,
    ) -> Result<Value, Legacy2024AdapterError> {
        let params =
            params
                .and_then(Value::as_object)
                .ok_or(Legacy2024AdapterError::invalid_params(
                    "initialize requires exact 2024 object params",
                ))?;
        if !params.get("protocolVersion").is_some_and(Value::is_string) {
            return Err(Legacy2024AdapterError::invalid_params(
                "initialize protocolVersion must be a string",
            ));
        }
        // An unsupported proposal still negotiates our supported version in
        // initialize_result. Reserved modern metadata is rejected separately.
        let client_capabilities =
            params
                .get("capabilities")
                .cloned()
                .ok_or(Legacy2024AdapterError::invalid_params(
                    "initialize requires client capabilities",
                ))?;
        let client_capabilities_bytes = serde_json::to_vec(&client_capabilities).map_err(|_| {
            Legacy2024AdapterError::invalid_params("initialize capabilities cannot be represented")
        })?;
        let client_capabilities = decode_legacy_2024_11_05_client_capabilities(client_capabilities)
            .map_err(|_| {
                Legacy2024AdapterError::invalid_params("initialize client capabilities are invalid")
            })?;
        let client_info = params.get("clientInfo").and_then(Value::as_object).ok_or(
            Legacy2024AdapterError::invalid_params("initialize requires clientInfo object"),
        )?;
        if !client_info.get("name").is_some_and(Value::is_string)
            || !client_info.get("version").is_some_and(Value::is_string)
        {
            return Err(Legacy2024AdapterError::invalid_params(
                "initialize clientInfo requires string name and version",
            ));
        }

        let result = initialize_result(&self.config)?;
        self.client_capabilities = Some(client_capabilities);
        self.client_capabilities_bytes = client_capabilities_bytes;
        self.client_info = Some(fastmcp_protocol::ClientInfo {
            name: client_info
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("unknown")
                .to_owned(),
            version: client_info
                .get("version")
                .and_then(Value::as_str)
                .unwrap_or("0")
                .to_owned(),
        });
        self.lifecycle = Legacy2024Lifecycle::AwaitInitialized;
        Ok(result)
    }

    fn require_server_capability(&self, method: &str) -> Result<(), Legacy2024AdapterError> {
        let admitted = match method {
            TOOLS_LIST | TOOLS_CALL => self.config.capabilities.tools.is_some(),
            RESOURCES_LIST | RESOURCES_TEMPLATES_LIST | RESOURCES_READ => {
                self.config.capabilities.resources.is_some()
            }
            PROMPTS_LIST | PROMPTS_GET => self.config.capabilities.prompts.is_some(),
            COMPLETION_COMPLETE => true,
            _ => false,
        };
        if admitted {
            Ok(())
        } else {
            Err(Legacy2024AdapterError::invalid_request(
                "negotiated server capabilities do not permit request",
            ))
        }
    }

    fn require_resource_subscribe_capability(&self) -> Result<(), Legacy2024AdapterError> {
        if self
            .config
            .capabilities
            .resources
            .as_ref()
            .is_some_and(|resources| resources.subscribe)
        {
            Ok(())
        } else {
            Err(Legacy2024AdapterError::invalid_request(
                "server resources.subscribe capability is required",
            ))
        }
    }

    fn subscribe(&mut self, params: Option<&Value>) -> Result<Value, Legacy2024AdapterError> {
        self.require_resource_subscribe_capability()?;
        let uri = uri_param(params)?;
        if !self.subscriptions.contains(uri) {
            self.reserve_state()?;
            self.subscriptions.insert(uri.to_owned());
        }
        Ok(json!({}))
    }

    fn unsubscribe(&mut self, params: Option<&Value>) -> Result<Value, Legacy2024AdapterError> {
        self.require_resource_subscribe_capability()?;
        let uri = uri_param(params)?;
        if self.subscriptions.remove(uri) {
            self.release_state()?;
        }
        Ok(json!({}))
    }

    fn set_logging_level(
        &mut self,
        params: Option<&Value>,
    ) -> Result<Value, Legacy2024AdapterError> {
        if self.config.capabilities.logging.is_none() {
            return Err(Legacy2024AdapterError::invalid_request(
                "server logging capability is required",
            ));
        }
        let level = params
            .and_then(Value::as_object)
            .and_then(|params| params.get("level"))
            .and_then(Value::as_str)
            .filter(|level| {
                matches!(
                    *level,
                    "alert"
                        | "critical"
                        | "debug"
                        | "emergency"
                        | "error"
                        | "info"
                        | "notice"
                        | "warning"
                )
            })
            .ok_or(Legacy2024AdapterError::invalid_params(
                "logging/setLevel requires an exact 2024 logging level",
            ))?;
        self.logging_level = Some(level.to_owned());
        Ok(json!({}))
    }

    fn client_supports(&self, capability: Legacy2024Capability) -> bool {
        let Some(capabilities) = self.client_capabilities.as_ref() else {
            return false;
        };
        match capability {
            Legacy2024Capability::ClientSampling => capabilities.sampling.is_some(),
            Legacy2024Capability::ClientRoots => capabilities.roots.is_some(),
            Legacy2024Capability::ClientRootsListChanged => capabilities
                .roots
                .as_ref()
                .is_some_and(|roots| roots.list_changed),
            _ => false,
        }
    }

    fn emit_reverse_request(
        &mut self,
        method: &'static str,
        params: Value,
    ) -> Result<Legacy2024Outbound, Legacy2024AdapterError> {
        let id = self.next_reverse_request_id;
        let next_reverse_request_id = self.next_reverse_request_id.checked_add(1).ok_or(
            Legacy2024AdapterError::invalid_request("legacy reverse request ID space is exhausted"),
        )?;
        self.reserve_state()?;
        self.next_reverse_request_id = next_reverse_request_id;
        self.pending_reverse_request_ids.insert(id);
        Ok(Legacy2024Outbound::ReverseRequest(json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params,
        })))
    }

    fn complete_reverse_request(&mut self, id: Value) -> Result<(), Legacy2024AdapterError> {
        if self.lifecycle != Legacy2024Lifecycle::Operating {
            return Err(Legacy2024AdapterError::invalid_request(
                "reverse response requires Operating lifecycle",
            ));
        }
        let id = id.as_i64().ok_or(Legacy2024AdapterError::invalid_request(
            "reverse response ID is not an adapter-owned integer",
        ))?;
        if !self.pending_reverse_request_ids.contains(&id) {
            return Err(Legacy2024AdapterError::invalid_request(
                "reverse response does not own an adapter request correlation",
            ));
        }
        self.release_state()?;
        self.pending_reverse_request_ids.remove(&id);
        Ok(())
    }

    fn reserve_state(&mut self) -> Result<(), Legacy2024AdapterError> {
        if self.reservation_count >= LEGACY_2024_MAX_ADAPTER_RESERVATIONS as u64 {
            return Err(Legacy2024AdapterError::invalid_request(
                "legacy adapter reservation limit reached",
            ));
        }
        self.reservation_count = self.reservation_count.checked_add(1).ok_or(
            Legacy2024AdapterError::invalid_request(
                "legacy adapter reservation count is exhausted",
            ),
        )?;
        Ok(())
    }

    fn release_state(&mut self) -> Result<(), Legacy2024AdapterError> {
        self.reservation_count = self.reservation_count.checked_sub(1).ok_or(
            Legacy2024AdapterError::invalid_request(
                "legacy adapter reservation accounting is inconsistent",
            ),
        )?;
        Ok(())
    }
}

fn initialize_result(config: &Legacy2024ServerConfig) -> Result<Value, Legacy2024AdapterError> {
    let mut result = json!({
        "protocolVersion": LEGACY_2024_11_05_PROTOCOL_VERSION,
        "capabilities": config.capabilities.clone(),
        "serverInfo": {
            "name": config.server_info.name.clone(),
            "version": config.server_info.version.clone(),
        },
    });
    if let Some(instructions) = &config.instructions {
        result["instructions"] = Value::String(instructions.clone());
    }
    validate_legacy_2024_11_05_initialize_result(&result).map_err(|_| {
        Legacy2024AdapterError::invalid_params(
            "configured server result is not exact MCP 2024-11-05",
        )
    })?;
    Ok(result)
}

fn uri_param(params: Option<&Value>) -> Result<&str, Legacy2024AdapterError> {
    params
        .and_then(Value::as_object)
        .and_then(|params| params.get("uri"))
        .and_then(Value::as_str)
        .ok_or(Legacy2024AdapterError::invalid_params(
            "resource subscription methods require string uri",
        ))
}

fn success_response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error_response(id: Value, error: Legacy2024AdapterError) -> Value {
    // Built without json!: embedding a JsonInteger through the macro
    // re-parses the number and canonicalizes its retained spelling.
    let mut error_object = serde_json::Map::new();
    error_object.insert("code".to_owned(), Value::Number(error.code().to_number()));
    error_object.insert(
        "message".to_owned(),
        Value::String(error.message().to_owned()),
    );
    let mut response = serde_json::Map::new();
    response.insert("jsonrpc".to_owned(), Value::String("2.0".to_owned()));
    response.insert("id".to_owned(), id);
    response.insert("error".to_owned(), Value::Object(error_object));
    Value::Object(response)
}

fn response_id_from_wire(wire: &Value) -> Option<Value> {
    let id = wire.as_object()?.get("id")?;
    if id.is_string()
        || id
            .as_number()
            .is_some_and(|number| JsonInteger::try_from_number(number.clone()).is_ok())
    {
        Some(id.clone())
    } else {
        None
    }
}

fn append_length_prefixed(bytes: &mut Vec<u8>, field: &[u8]) {
    bytes.extend_from_slice(&(field.len() as u32).to_be_bytes());
    bytes.extend_from_slice(field);
}

/// Encodes the canonical LEG-02 A receipt digest preimage without hashing it.
#[must_use]
pub fn legacy_2024_a_digest_preimage(
    ordinal: u32,
    group: &[u8],
    input_lifecycle: Legacy2024Lifecycle,
    wire: &[u8],
    capabilities: &[u8],
    method: &[u8],
    direction: Legacy2024Direction,
    output_lifecycle: Legacy2024Lifecycle,
    state_digest: &[u8],
) -> Vec<u8> {
    let mut bytes = b"fastmcp-leg-02-a-v1\0".to_vec();
    for field in [
        ordinal.to_be_bytes().to_vec(),
        group.to_vec(),
        lifecycle_bytes(input_lifecycle).to_vec(),
        wire.to_vec(),
        capabilities.to_vec(),
        method.to_vec(),
        direction_bytes(direction).to_vec(),
        lifecycle_bytes(output_lifecycle).to_vec(),
        state_digest.to_vec(),
    ] {
        append_length_prefixed(&mut bytes, &field);
    }
    bytes
}

/// Encodes the canonical LEG-02 B operating-isolation and teardown digest
/// preimage without hashing it.
#[must_use]
pub fn legacy_2024_b_digest_preimage(
    partition_ordinal: u32,
    operation_ordinal: u32,
    method: &[u8],
    direction: Legacy2024Direction,
    owner_partition: &[u8],
    connection_identity: &[u8],
    before_state_digest: &[u8],
    after_state_digest: &[u8],
    lifecycle: Legacy2024Lifecycle,
    reservation_count: u64,
    release_counter: u64,
) -> Vec<u8> {
    let mut bytes = b"fastmcp-leg-02-b-v1\0".to_vec();
    for field in [
        partition_ordinal.to_be_bytes().to_vec(),
        operation_ordinal.to_be_bytes().to_vec(),
        method.to_vec(),
        direction_bytes(direction).to_vec(),
        owner_partition.to_vec(),
        connection_identity.to_vec(),
        before_state_digest.to_vec(),
        after_state_digest.to_vec(),
        lifecycle_bytes(lifecycle).to_vec(),
        reservation_count.to_be_bytes().to_vec(),
        release_counter.to_be_bytes().to_vec(),
    ] {
        append_length_prefixed(&mut bytes, &field);
    }
    bytes
}

const fn lifecycle_bytes(lifecycle: Legacy2024Lifecycle) -> &'static [u8] {
    match lifecycle {
        Legacy2024Lifecycle::AwaitInitialize => b"AwaitInitialize",
        Legacy2024Lifecycle::AwaitInitialized => b"AwaitInitialized",
        Legacy2024Lifecycle::Operating => b"Operating",
        Legacy2024Lifecycle::Closed => b"Closed",
    }
}

const fn direction_bytes(direction: Legacy2024Direction) -> &'static [u8] {
    match direction {
        Legacy2024Direction::ClientToServer => b"ClientToServer",
        Legacy2024Direction::ServerToClient => b"ServerToClient",
        Legacy2024Direction::Bidirectional => b"Bidirectional",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    use fastmcp_protocol::methods::{
        Legacy2024ListChangedCapability, Legacy2024ResourcesCapability,
    };

    #[derive(Default)]
    struct RecordingHandler {
        methods: Vec<&'static str>,
    }

    impl Legacy2024Handler for RecordingHandler {
        fn handle_legacy_2024(
            &mut self,
            method: &'static str,
            _params: Option<&Value>,
        ) -> Result<Value, Legacy2024HandlerError> {
            self.methods.push(method);
            Ok(json!({"handled": method}))
        }
    }

    struct FailingHandler {
        error: Legacy2024HandlerError,
    }

    impl Legacy2024Handler for FailingHandler {
        fn handle_legacy_2024(
            &mut self,
            _method: &'static str,
            _params: Option<&Value>,
        ) -> Result<Value, Legacy2024HandlerError> {
            Err(self.error.clone())
        }
    }

    struct StaticResultHandler {
        result: Value,
    }

    impl Legacy2024Handler for StaticResultHandler {
        fn handle_legacy_2024(
            &mut self,
            _method: &'static str,
            _params: Option<&Value>,
        ) -> Result<Value, Legacy2024HandlerError> {
            Ok(self.result.clone())
        }
    }

    struct LiveRecordingHandler {
        binding: LegacyPeerBinding,
        calls: Arc<Mutex<Vec<(LegacyPeerBinding, &'static str)>>>,
    }

    impl Legacy2024Handler for LiveRecordingHandler {
        fn handle_legacy_2024(
            &mut self,
            method: &'static str,
            _params: Option<&Value>,
        ) -> Result<Value, Legacy2024HandlerError> {
            self.calls
                .lock()
                .expect("live handler call log must not be poisoned")
                .push((self.binding, method));
            Ok(json!({
                "bindingGeneration": self.binding.generation(),
                "handled": method,
            }))
        }
    }

    const TEST_TRANSPORT_PARTITION: LegacyAuthenticatedPeerPartition =
        LegacyAuthenticatedPeerPartition::from_authenticated_transport([7; 32]);

    fn binding() -> LegacyPeerBinding {
        binding_for(TEST_TRANSPORT_PARTITION, 7)
    }

    fn binding_for(
        partition: LegacyAuthenticatedPeerPartition,
        generation: u64,
    ) -> LegacyPeerBinding {
        LegacyPeerBinding::from_authenticated_transport(partition, generation)
    }

    fn server_config() -> Legacy2024ServerConfig {
        Legacy2024ServerConfig {
            capabilities: Legacy2024ServerCapabilities {
                logging: Some(BTreeMap::default()),
                tools: Some(Legacy2024ListChangedCapability::default()),
                resources: Some(Legacy2024ResourcesCapability {
                    subscribe: true,
                    ..Legacy2024ResourcesCapability::default()
                }),
                prompts: Some(Legacy2024ListChangedCapability::default()),
                ..Legacy2024ServerCapabilities::default()
            },
            server_info: Legacy2024ServerInfo {
                name: "legacy-server".to_owned(),
                version: "1.0.0".to_owned(),
            },
            instructions: Some("exact legacy profile".to_owned()),
        }
    }

    fn adapter() -> Legacy2024ServerAdapter<RecordingHandler> {
        Legacy2024ServerAdapter::install(binding(), server_config(), RecordingHandler::default())
            .expect("exact test configuration must install")
    }

    fn failing_adapter(error: Legacy2024HandlerError) -> Legacy2024ServerAdapter<FailingHandler> {
        Legacy2024ServerAdapter::install(binding(), server_config(), FailingHandler { error })
            .expect("exact test configuration must install")
    }

    fn tool_result_adapter(result: Value) -> Legacy2024ServerAdapter<StaticResultHandler> {
        Legacy2024ServerAdapter::install(binding(), server_config(), StaticResultHandler { result })
            .expect("exact test configuration must install")
    }

    fn initialize() -> Value {
        json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "protocolVersion": "2024-11-05",
                "capabilities": {
                    "sampling": {},
                    "roots": {"listChanged": true},
                    "elicitation": {"form": {}}
                },
                "clientInfo": {"name": "legacy-client", "version": "1.0.0"}
            }
        })
    }

    fn initialize_operating<H: Legacy2024Handler>(adapter: &mut Legacy2024ServerAdapter<H>) {
        adapter
            .receive(binding(), initialize())
            .expect("initialize must be admitted");
        adapter
            .receive(
                binding(),
                json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED}),
            )
            .expect("initialized notification must be admitted");
    }

    #[test]
    fn handler_error_preserves_arbitrary_width_code_lexeme() {
        let raw_code = "340282366920938463463374607431768211457";
        let code = raw_code
            .parse::<JsonInteger>()
            .expect("huge integer error code must be admitted");
        let mut adapter = failing_adapter(Legacy2024HandlerError::with_code(code, "failed"));
        initialize_operating(&mut adapter);

        let response = adapter
            .receive(
                binding(),
                json!({"jsonrpc": "2.0", "id": 2, "method": TOOLS_LIST}),
            )
            .expect("handler failure must become a response");
        let Legacy2024Outbound::Response(response) = response else {
            panic!("handler failure must become a JSON-RPC error response");
        };

        assert_eq!(
            response["error"]["code"]
                .as_number()
                .expect("error code must remain a JSON number")
                .as_str(),
            raw_code
        );
    }

    #[test]
    fn handler_error_code_classifies_standard_values_mathematically() {
        let code = "-3.2603e4"
            .parse::<JsonInteger>()
            .expect("mathematically integral standard code must be admitted");
        let mut adapter = failing_adapter(Legacy2024HandlerError::with_code(code, "failed"));
        initialize_operating(&mut adapter);

        let response = adapter
            .receive(
                binding(),
                json!({"jsonrpc": "2.0", "id": 2, "method": TOOLS_LIST}),
            )
            .expect("handler failure must become a response");
        let Legacy2024Outbound::Response(response) = response else {
            panic!("handler failure must become a JSON-RPC error response");
        };
        let code = JsonInteger::try_from_number(
            response["error"]["code"]
                .as_number()
                .expect("error code must remain a JSON number")
                .clone(),
        )
        .expect("emitted error code must remain mathematically integral");

        assert_eq!(code.as_i32(), Some(-32603));
        assert_eq!(code.as_str(), "-3.2603e4");
    }

    #[test]
    fn handler_error_rejects_fractional_negative_code() {
        assert!(
            "-340282366920938463463374607431768211457.5"
                .parse::<JsonInteger>()
                .is_err()
        );
    }

    #[test]
    fn lifecycle_rows_cover_exact_2024_adapter() {
        let binding = binding();
        let mut adapter = adapter();
        let mut lifecycle_rows = Vec::new();

        lifecycle_rows.push(adapter.receive(binding, initialize()).unwrap());
        lifecycle_rows.push(
            adapter
                .receive(
                    binding,
                    json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
                )
                .unwrap(),
        );
        lifecycle_rows.push(
            adapter
                .receive(
                    binding,
                    json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
                )
                .unwrap(),
        );
        lifecycle_rows.push(
            adapter
                .receive(
                    binding,
                    json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"}),
                )
                .unwrap(),
        );
        lifecycle_rows.push(
            adapter
                .receive(
                    binding,
                    json!({"jsonrpc": "2.0", "id": 4, "method": "prompts/list"}),
                )
                .unwrap(),
        );
        lifecycle_rows.push(
            adapter
                .receive(
                    binding,
                    json!({
                        "jsonrpc": "2.0", "id": 5, "method": "completion/complete",
                        "params": {
                            "ref": {"type": "ref/prompt", "name": "legacy-prompt"},
                            "argument": {"name": "topic", "value": "leg"},
                        },
                    }),
                )
                .unwrap(),
        );
        lifecycle_rows.push(adapter.receive(binding, json!({"jsonrpc": "2.0", "id": 6, "method": "resources/subscribe", "params": {"uri": "file:///workspace"}})).unwrap());
        lifecycle_rows.push(
            adapter
                .make_reverse_request(binding, ROOTS_LIST, json!({}))
                .unwrap(),
        );
        lifecycle_rows.push(
            adapter
                .make_reverse_request(
                    binding,
                    SAMPLING_CREATE_MESSAGE,
                    json!({"messages": [], "maxTokens": 16}),
                )
                .unwrap(),
        );
        lifecycle_rows.push(adapter.receive(binding, json!({"jsonrpc": "2.0", "id": 7, "method": "logging/setLevel", "params": {"level": "info"}})).unwrap());
        lifecycle_rows.push(adapter.receive(binding, json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": 6}})).unwrap());
        lifecycle_rows.push(adapter.receive(binding, json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": {"progressToken": 1, "progress": 1}})).unwrap());

        assert_eq!(lifecycle_rows.len(), 12);
        assert_eq!(
            lifecycle_rows[0],
            Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0", "id": 1,
                "result": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {
                        "logging": {},
                        "prompts": {},
                        "resources": {"subscribe": true},
                        "tools": {},
                    },
                    "serverInfo": {"name": "legacy-server", "version": "1.0.0"},
                    "instructions": "exact legacy profile",
                },
            }))
        );
        assert_eq!(lifecycle_rows[1], Legacy2024Outbound::NoResponse);
        for (row, id, method) in [
            (2, 2, TOOLS_LIST),
            (3, 3, RESOURCES_LIST),
            (4, 4, PROMPTS_LIST),
            (5, 5, COMPLETION_COMPLETE),
        ] {
            assert_eq!(
                lifecycle_rows[row],
                Legacy2024Outbound::Response(json!({
                    "jsonrpc": "2.0", "id": id, "result": {"handled": method},
                }))
            );
        }
        assert_eq!(
            lifecycle_rows[6],
            Legacy2024Outbound::Response(json!({"jsonrpc": "2.0", "id": 6, "result": {}}))
        );
        assert_eq!(
            lifecycle_rows[7],
            Legacy2024Outbound::ReverseRequest(
                json!({"jsonrpc": "2.0", "id": 1, "method": ROOTS_LIST, "params": {}})
            )
        );
        assert_eq!(
            lifecycle_rows[8],
            Legacy2024Outbound::ReverseRequest(json!({
                "jsonrpc": "2.0", "id": 2,
                "method": SAMPLING_CREATE_MESSAGE,
                "params": {"messages": [], "maxTokens": 16},
            }))
        );
        assert_eq!(
            lifecycle_rows[9],
            Legacy2024Outbound::Response(json!({"jsonrpc": "2.0", "id": 7, "result": {}}))
        );
        assert_eq!(lifecycle_rows[10], Legacy2024Outbound::NoResponse);
        assert_eq!(lifecycle_rows[11], Legacy2024Outbound::NoResponse);
        assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::Operating);
        assert_eq!(adapter.snapshot().operating_transition_count, 1);
        assert_eq!(adapter.snapshot().control_notification_count, 2);
        assert_eq!(adapter.snapshot().subscriptions, ["file:///workspace"]);
        assert_eq!(
            adapter.handler.methods,
            vec![
                TOOLS_LIST,
                RESOURCES_LIST,
                PROMPTS_LIST,
                COMPLETION_COMPLETE,
                RESOURCES_SUBSCRIBE,
            ]
        );
    }

    #[test]
    fn unknown_request_method_is_method_not_found() {
        let binding = binding();
        let mut adapter = adapter();
        initialize_operating(&mut adapter);
        let before = adapter.snapshot();

        let response = adapter
            .receive(
                binding,
                json!({
                    "jsonrpc": "2.0",
                    "id": 41,
                    "method": "totally/unknown/method"
                }),
            )
            .expect("an id-bearing unknown method must receive a JSON-RPC error response");
        let Legacy2024Outbound::Response(response) = response else {
            panic!("unknown request method must produce a response");
        };
        // JSON-RPC 2.0: a structurally valid request naming an unavailable
        // method is Method Not Found (-32601), not Invalid Request (-32600).
        assert_eq!(response["error"]["code"], -32601);
        assert_eq!(response["id"], 41);
        assert!(
            response["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("method"),
            "refusal must name the method taxonomy: {response}"
        );
        assert_eq!(adapter.snapshot(), before);
        assert_eq!(adapter.handler.methods.len(), 0);
    }

    #[test]
    fn initialize_negotiates_proposals_and_keeps_legacy_session_operational() {
        for proposal in ["2024-11-05", "2025-03-26", "2025-06-18", "2099-01-01"] {
            let mut adapter = adapter();
            let mut request = initialize();
            request["params"]["protocolVersion"] = json!(proposal);
            let Legacy2024Outbound::Response(response) = adapter
                .receive(binding(), request)
                .expect("initialize response")
            else {
                panic!("initialize must return a response");
            };
            assert_eq!(
                response["result"]["protocolVersion"], "2024-11-05",
                "{proposal}"
            );
            assert!(response.get("error").is_none(), "{response}");
            adapter
                .receive(
                    binding(),
                    json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED}),
                )
                .expect("negotiated session must become operational");
            assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::Operating);
            let Legacy2024Outbound::Response(response) = adapter
                .receive(
                    binding(),
                    json!({"jsonrpc": "2.0", "id": 2, "method": TOOLS_LIST}),
                )
                .expect("tools/list response")
            else {
                panic!("tools/list must return a response");
            };
            assert_eq!(response["result"]["handled"], TOOLS_LIST);
            assert_eq!(adapter.handler.methods, [TOOLS_LIST]);

            let before = adapter.snapshot();
            let Legacy2024Outbound::Response(response) = adapter
                .receive(binding(), json!({
                    "jsonrpc": "2.0", "id": 3, "method": TOOLS_LIST,
                    "params": {"_meta": {"io.modelcontextprotocol/protocolVersion": "2026-07-28"}}
                }))
                .expect("mixed-era request must receive an error")
            else {
                panic!("mixed-era request must return a response");
            };
            assert_eq!(response["error"]["code"], -32602);
            assert_eq!(
                response["error"]["message"],
                "invalid exact MCP 2024-11-05 parameters"
            );
            assert_eq!(adapter.snapshot(), before);
            assert_eq!(adapter.handler.methods, [TOOLS_LIST]);
        }
    }

    #[test]
    fn malformed_initialize_version_preserves_adapter_state() {
        let binding = binding();
        let mut adapter = adapter();
        let before = adapter.snapshot();

        let mut malformed = initialize();
        malformed["params"]["protocolVersion"] = json!(2025);
        let response = adapter.receive(binding, malformed).unwrap();
        let Legacy2024Outbound::Response(response) = response else {
            panic!("invalid initialize request must receive a JSON-RPC error response");
        };
        assert_eq!(response["error"]["code"], -32600);
        assert_eq!(
            response["error"]["message"],
            "invalid exact MCP 2024-11-05 envelope"
        );
        assert_eq!(adapter.snapshot(), before);
        assert_eq!(adapter.handler.methods.len(), 0);
    }

    #[test]
    fn resource_update_notification_requires_the_exact_subscribed_uri() {
        let binding = binding();
        let subscribed_uri = "file:///workspace/subscribed";
        let mut adapter = adapter();
        initialize_operating(&mut adapter);
        assert_eq!(
            adapter.receive(
                binding,
                json!({
                    "jsonrpc": "2.0", "id": 2, "method": RESOURCES_SUBSCRIBE,
                    "params": {"uri": subscribed_uri},
                }),
            ),
            Ok(Legacy2024Outbound::Response(
                json!({"jsonrpc": "2.0", "id": 2, "result": {}})
            ))
        );
        let before = adapter.snapshot();

        assert_eq!(
            adapter.make_notification(
                binding,
                NOTIFICATIONS_RESOURCES_UPDATED,
                Some(json!({"uri": subscribed_uri})),
            ),
            Ok(Legacy2024Outbound::ReverseNotification(json!({
                "jsonrpc": "2.0",
                "method": NOTIFICATIONS_RESOURCES_UPDATED,
                "params": {"uri": subscribed_uri},
            })))
        );
        assert_eq!(adapter.snapshot(), before);

        let error = adapter
            .make_notification(
                binding,
                NOTIFICATIONS_RESOURCES_UPDATED,
                Some(json!({"uri": "file:///workspace/not-subscribed"})),
            )
            .expect_err("changing only the URI must not notify an unsubscribed resource");
        assert_eq!(error.code().as_i32(), Some(-32600));
        assert_eq!(adapter.snapshot(), before);
        assert_eq!(adapter.handler.methods, [RESOURCES_SUBSCRIBE]);
    }

    #[test]
    fn final_reserved_request_metadata_rejects_before_legacy_adapter_state_changes() {
        let binding = binding();
        let mut adapter = adapter();
        let before = adapter.snapshot();

        for member in [
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/serverInfo",
            "io.modelcontextprotocol/subscriptionId",
        ] {
            let mut rejected = initialize();
            rejected["params"]["_meta"][member] = json!({});
            let response = adapter
                .receive(binding, rejected)
                .expect("invalid request metadata receives a JSON-RPC error response");
            let Legacy2024Outbound::Response(response) = response else {
                panic!("request metadata rejection must not become a notification result");
            };
            assert_eq!(response["error"]["code"], -32600, "{member}");
            assert_eq!(adapter.snapshot(), before, "{member}");
        }
        assert_eq!(adapter.handler.methods.len(), 0);
    }

    #[test]
    fn final_reserved_handler_result_metadata_rejects_without_adapter_state_change() {
        let binding = binding();
        let accepted_result = json!({
            "content": [{"type": "text", "text": "legacy"}],
            "_meta": {"com.example/application": true}
        });
        let request = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": TOOLS_CALL,
            "params": {"name": "legacy-tool"}
        });

        let mut accepted = tool_result_adapter(accepted_result.clone());
        initialize_operating(&mut accepted);
        assert_eq!(
            accepted.receive(binding, request.clone()),
            Ok(Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0", "id": 2, "result": accepted_result,
            })))
        );

        let mut rejected_result = accepted_result;
        rejected_result["_meta"]["io.modelcontextprotocol/serverInfo"] = json!({});
        let mut rejected = tool_result_adapter(rejected_result);
        initialize_operating(&mut rejected);
        let before = rejected.snapshot();
        assert_eq!(
            rejected.receive(binding, request),
            Ok(Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0",
                "id": 2,
                "error": {
                    "code": -32603,
                    "message": "handler result is not losslessly representable in exact MCP 2024-11-05",
                }
            })))
        );
        assert_eq!(rejected.snapshot(), before);
    }

    #[test]
    fn final_result_members_do_not_complete_pending_legacy_reverse_requests() {
        let binding = binding();
        let mut adapter = adapter();
        initialize_operating(&mut adapter);
        let Legacy2024Outbound::ReverseRequest(accepted_request) = adapter
            .make_reverse_request(binding, PING, json!({}))
            .expect("operating adapter admits a ping reverse request")
        else {
            panic!("ping must create a reverse request");
        };
        let accepted_id = accepted_request["id"].clone();
        assert!(matches!(
            adapter.receive(
                binding,
                json!({
                    "jsonrpc": "2.0",
                    "id": accepted_id,
                    "result": {
                        "legacy": true,
                        "_meta": {"com.example/application": true}
                    }
                })
            ),
            Ok(Legacy2024Outbound::NoResponse)
        ));

        for member in std::iter::once("resultType").chain([
            "io.modelcontextprotocol/protocolVersion",
            "io.modelcontextprotocol/clientCapabilities",
            "io.modelcontextprotocol/clientInfo",
            "io.modelcontextprotocol/serverInfo",
            "io.modelcontextprotocol/subscriptionId",
        ]) {
            let Legacy2024Outbound::ReverseRequest(request) = adapter
                .make_reverse_request(binding, PING, json!({}))
                .expect("each planted negative receives a fresh pending request")
            else {
                panic!("ping must create a reverse request");
            };
            let mut rejected = json!({
                "jsonrpc": "2.0",
                "id": request["id"].clone(),
                "result": {
                    "legacy": true,
                    "_meta": {"com.example/application": true}
                }
            });
            if member == "resultType" {
                rejected["result"]["resultType"] = json!("complete");
            } else {
                rejected["result"]["_meta"][member] = json!({});
            }
            let before = adapter.snapshot();
            let error = adapter
                .receive(binding, rejected)
                .expect_err("a response-shaped rejection must not receive a response");
            assert_eq!(error.code().as_i32(), Some(-32600), "{member}");
            assert_eq!(adapter.snapshot(), before, "{member}");
        }
    }

    #[test]
    fn mixed_response_shapes_preserve_pending_reverse_and_lifecycle_state() {
        let binding = binding();
        let mut adapter = adapter();
        initialize_operating(&mut adapter);
        let Legacy2024Outbound::ReverseRequest(reverse_request) = adapter
            .make_reverse_request(binding, PING, json!({}))
            .expect("operating adapter admits a ping reverse request")
        else {
            panic!("ping must create a reverse request");
        };
        let pending_id = reverse_request["id"].clone();

        for rejected in [
            json!({
                "jsonrpc": "2.0",
                "id": pending_id.clone(),
                "method": null,
                "result": {
                    "legacy": true,
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28"
                    }
                }
            }),
            json!({
                "jsonrpc": "2.0",
                "id": pending_id.clone(),
                "method": PING,
                "result": {"legacy": true}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": pending_id.clone(),
                "result": {},
                "error": {"code": -32603, "message": "failed"}
            }),
            json!({
                "jsonrpc": "2.0",
                "id": pending_id.clone(),
                "params": {}
            }),
        ] {
            let before = adapter.snapshot();
            let handler_methods_before = adapter.handler.methods.clone();
            let error = adapter
                .receive(binding, rejected)
                .expect_err("a response-shaped rejection must not receive a response");
            assert_eq!(error.code().as_i32(), Some(-32600));
            assert_eq!(adapter.snapshot(), before);
            assert_eq!(adapter.handler.methods, handler_methods_before);
        }

        assert_eq!(
            adapter.receive(
                binding,
                json!({"jsonrpc": "2.0", "id": "legal-request", "method": PING})
            ),
            Ok(Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0",
                "id": "legal-request",
                "result": {}
            })))
        );
        assert!(matches!(
            adapter.receive(
                binding,
                json!({"jsonrpc": "2.0", "id": pending_id, "result": {}})
            ),
            Ok(Legacy2024Outbound::NoResponse)
        ));
        assert_eq!(adapter.snapshot().pending_reverse_request_ids.len(), 0);
        assert_eq!(adapter.lifecycle(), Legacy2024Lifecycle::Operating);
    }

    #[test]
    fn preadmission_error_responses_preserve_integral_request_id_lexemes() {
        let binding = binding();
        let mut adapter = adapter();

        for raw_id in ["7e2", "340282366920938463463374607431768211457"] {
            let wire: Value = serde_json::from_str(&format!(
                r#"{{"jsonrpc":"2.0","id":{raw_id},"method":"tools/list","params":false}}"#
            ))
            .expect("integral request-id wire must parse");
            // serde_json canonicalizes exponent spellings while parsing the
            // wire ("7e2" arrives as "7e+2"); the adapter's obligation is to
            // echo the parsed id without any further precision or lexeme
            // loss, which keeps beyond-u64 integers exact.
            let parsed_id = wire["id"]
                .as_number()
                .expect("request ID parses as a JSON number")
                .as_str()
                .to_owned();
            let response = adapter
                .receive(binding, wire)
                .expect("a valid request ID must receive the pre-admission error response");
            let Legacy2024Outbound::Response(response) = response else {
                panic!("pre-admission request failure must have a JSON-RPC response");
            };

            assert_eq!(
                response["id"]
                    .as_number()
                    .expect("response ID remains a JSON number")
                    .as_str(),
                parsed_id
            );
            assert_eq!(response["error"]["code"], -32600);
        }
    }

    #[test]
    fn preadmission_error_responses_reject_fractional_request_ids() {
        let binding = binding();
        let mut adapter = adapter();
        let before = adapter.snapshot();
        let wire: Value = serde_json::from_str(
            r#"{"jsonrpc":"2.0","id":340282366920938463463374607431768211457.5,"method":"tools/list","params":{}}"#,
        )
        .expect("fractional request-id wire must parse");

        let error = adapter
            .receive(binding, wire)
            .expect_err("fractional request IDs must not receive a pre-admission response");
        assert_eq!(error.code().as_i32(), Some(-32600));
        assert_eq!(adapter.snapshot(), before);
        assert_eq!(adapter.handler.methods.len(), 0);
    }

    #[test]
    fn leg_02_i_positive() {
        let left = binding_for(
            LegacyAuthenticatedPeerPartition::from_authenticated_transport([0x11; 32]),
            101,
        );
        let right = binding_for(
            LegacyAuthenticatedPeerPartition::from_authenticated_transport([0x22; 32]),
            202,
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler_calls = Arc::clone(&calls);
        let mut lifecycle =
            Legacy2024LiveServerLifecycle::new(server_config(), 2, move |binding| {
                LiveRecordingHandler {
                    binding,
                    calls: Arc::clone(&handler_calls),
                }
            })
            .expect("bounded exact-2024 live lifecycle must install");

        assert_eq!(lifecycle.max_live_bindings(), 2);
        assert!(lifecycle.install(left).unwrap().matches_binding(left));
        assert!(lifecycle.install(right).unwrap().matches_binding(right));
        assert_eq!(lifecycle.active_binding_count(), 2);

        for binding in [left, right] {
            assert!(matches!(
                lifecycle.receive(binding, initialize()),
                Ok(Legacy2024Outbound::Response(_))
            ));
            assert_eq!(
                lifecycle.receive(
                    binding,
                    json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED}),
                ),
                Ok(Legacy2024Outbound::NoResponse)
            );
        }

        assert_eq!(
            lifecycle.receive(
                left,
                json!({"jsonrpc": "2.0", "id": 2, "method": TOOLS_LIST}),
            ),
            Ok(Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0", "id": 2,
                "result": {"bindingGeneration": 101, "handled": TOOLS_LIST},
            })))
        );
        assert_eq!(
            lifecycle.receive(
                right,
                json!({"jsonrpc": "2.0", "id": 3, "method": TOOLS_LIST}),
            ),
            Ok(Legacy2024Outbound::Response(json!({
                "jsonrpc": "2.0", "id": 3,
                "result": {"bindingGeneration": 202, "handled": TOOLS_LIST},
            })))
        );
        assert_eq!(
            lifecycle.receive(
                left,
                json!({
                    "jsonrpc": "2.0", "id": 4, "method": RESOURCES_SUBSCRIBE,
                    "params": {"uri": "file:///left-only"},
                }),
            ),
            Ok(Legacy2024Outbound::Response(
                json!({"jsonrpc": "2.0", "id": 4, "result": {}})
            ))
        );
        assert_eq!(
            lifecycle.make_reverse_request(left, PING, json!({})),
            Ok(Legacy2024Outbound::ReverseRequest(
                json!({"jsonrpc": "2.0", "id": 1, "method": PING, "params": {}})
            ))
        );
        assert_eq!(
            lifecycle.receive(left, json!({"jsonrpc": "2.0", "id": 1, "result": {}})),
            Ok(Legacy2024Outbound::NoResponse)
        );

        let left_before_close = lifecycle.snapshot(left).unwrap();
        let right_before_close = lifecycle.snapshot(right).unwrap();
        assert_eq!(left_before_close.subscriptions, ["file:///left-only"]);
        assert_eq!(left_before_close.reservation_count, 1);
        assert_eq!(right_before_close.subscriptions.len(), 0);
        assert_eq!(right_before_close.reservation_count, 0);

        let left_closed = lifecycle.close(left).unwrap();
        assert_eq!(left_closed.lifecycle, Legacy2024Lifecycle::Closed);
        assert_eq!(left_closed.reservation_count, 0);
        assert_eq!(left_closed.close_release_count, 1);
        assert_eq!(lifecycle.active_binding_count(), 1);
        assert_eq!(lifecycle.snapshot(right).unwrap(), right_before_close);

        let right_closed = lifecycle.close(right).unwrap();
        assert_eq!(right_closed.lifecycle, Legacy2024Lifecycle::Closed);
        assert_eq!(right_closed.reservation_count, 0);
        assert_eq!(right_closed.close_release_count, 1);
        assert_eq!(lifecycle.active_binding_count(), 0);
        assert_eq!(
            *calls
                .lock()
                .expect("live handler call log must not be poisoned"),
            vec![
                (left, TOOLS_LIST),
                (right, TOOLS_LIST),
                (left, RESOURCES_SUBSCRIBE),
            ]
        );
    }

    #[test]
    fn leg_02_i_planted_negative() {
        let left = binding_for(
            LegacyAuthenticatedPeerPartition::from_authenticated_transport([0x31; 32]),
            303,
        );
        let right = binding_for(
            LegacyAuthenticatedPeerPartition::from_authenticated_transport([0x32; 32]),
            404,
        );
        let wrong_owner = binding_for(
            LegacyAuthenticatedPeerPartition::from_authenticated_transport([0x33; 32]),
            303,
        );
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler_calls = Arc::clone(&calls);
        let mut lifecycle =
            Legacy2024LiveServerLifecycle::new(server_config(), 2, move |binding| {
                LiveRecordingHandler {
                    binding,
                    calls: Arc::clone(&handler_calls),
                }
            })
            .expect("bounded exact-2024 live lifecycle must install");

        for binding in [left, right] {
            lifecycle.install(binding).unwrap();
            lifecycle.receive(binding, initialize()).unwrap();
            lifecycle
                .receive(
                    binding,
                    json!({"jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED}),
                )
                .unwrap();
        }
        let left_before = lifecycle.snapshot(left).unwrap();
        let right_before = lifecycle.snapshot(right).unwrap();

        let error = lifecycle
            .receive(wrong_owner, initialize())
            .expect_err("changing only the authenticated owner must not select a live lifecycle");
        assert_eq!(error.code().as_i32(), Some(-32600));
        assert_eq!(
            error.message(),
            "legacy peer binding has no live adapter lifecycle"
        );
        assert_eq!(lifecycle.snapshot(left).unwrap(), left_before);
        assert_eq!(lifecycle.snapshot(right).unwrap(), right_before);
        assert_eq!(lifecycle.active_binding_count(), 2);
        assert!(
            calls
                .lock()
                .expect("live handler call log must not be poisoned")
                .is_empty()
        );

        assert_eq!(lifecycle.close(left).unwrap().close_release_count, 1);
        assert_eq!(lifecycle.close(right).unwrap().close_release_count, 1);
        assert_eq!(lifecycle.active_binding_count(), 0);
    }
}
