//! Bidirectional request handling for server-to-client communication.
//!
//! This module provides the infrastructure for server-initiated requests to clients,
//! such as:
//! - `sampling/createMessage` - Request LLM completion from the client
//! - `elicitation/create` - Request user input from the client
//! - `roots/list` - Request filesystem roots from the client
//!
//! # Architecture
//!
//! The MCP protocol is bidirectional: while clients typically send requests to servers,
//! servers can also send requests to clients. This creates a challenge because the
//! server's main loop is typically blocking on `recv()`.
//!
//! The solution is a message dispatcher pattern:
//! 1. A background task continuously reads from the transport
//! 2. Incoming messages are routed based on whether they're requests or responses
//! 3. Responses are matched to pending requests via their ID
//! 4. Requests are dispatched to handlers
//!
//! # Usage
//!
//! ```ignore
//! // Send a request and await the response
//! let response = request_sender.send_request(
//!     &cx,
//!     "sampling/createMessage",
//!     params,
//! ).await?;
//! ```

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::future::{Future, poll_fn};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::channel::oneshot;
use asupersync::channel::oneshot::RecvError;
use base64::Engine as _;
use fastmcp_core::{
    ClientRoot, ElicitationAction, ElicitationMode, ElicitationRequest, ElicitationResponse,
    ElicitationSender, McpContext, McpError, McpErrorCode, McpRequestCancellation, McpResult,
    RootsProvider, SamplingRequest, SamplingResponse, SamplingRole, SamplingSender,
    SamplingStopReason, draw_security_identifier,
};
use fastmcp_protocol::protocol_policy::ProtocolEra;
use fastmcp_protocol::{
    CorrelationKey, FinalInputResponses, JsonRpcError, JsonRpcMessage, JsonRpcRequest,
    JsonRpcResponse, RequestId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde::ser::{SerializeMap, SerializeStruct};

/// Default maximum number of concurrent server-to-client requests.
pub const DEFAULT_MAX_IN_FLIGHT_REQUESTS: usize = 1_024;

/// Absolute maximum accepted by [`PendingRequests::with_max_in_flight`].
pub const HARD_MAX_IN_FLIGHT_REQUESTS: usize = 16_384;

/// Default maximum rounds for a single final MRTR exchange.
pub const DEFAULT_MAX_MRTR_ROUNDS: u8 = 8;

/// Absolute maximum rounds a server-local MRTR exchange may use.
pub const HARD_MAX_MRTR_ROUNDS: u8 = 32;

/// Default maximum embedded input requests in one MRTR result.
pub const DEFAULT_MAX_MRTR_INPUT_REQUESTS_PER_ROUND: usize = 32;

/// Absolute maximum embedded input requests in one MRTR result.
pub const HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND: usize = 128;

/// Default maximum embedded input requests across one complete MRTR exchange.
pub const DEFAULT_MAX_MRTR_INPUT_REQUESTS_TOTAL: usize = 128;

/// Absolute maximum embedded input requests across one complete MRTR exchange.
pub const HARD_MAX_MRTR_INPUT_REQUESTS_TOTAL: usize = 512;

/// Default lifetime for an MRTR request-state record.
pub const DEFAULT_MRTR_REQUEST_STATE_TTL: Duration = Duration::from_mins(15);

/// Absolute maximum lifetime for an MRTR request-state record.
pub const HARD_MAX_MRTR_REQUEST_STATE_TTL: Duration = Duration::from_hours(1);

/// Default number of retained, process-local MRTR request-state records.
pub const DEFAULT_MAX_MRTR_REQUEST_STATES: usize = 4_096;

/// Absolute maximum number of retained, process-local MRTR request-state records.
pub const HARD_MAX_MRTR_REQUEST_STATES: usize = 65_536;

/// Default maximum encoded request-state bytes admitted from an MRTR retry.
pub const DEFAULT_MAX_MRTR_REQUEST_STATE_BYTES: usize = 64 * 1024;

/// Maximum encoded request-state bytes admitted from an MRTR retry.
pub const HARD_MAX_MRTR_REQUEST_STATE_BYTES: usize = 256 * 1024;

const FIRST_SERVER_REQUEST_ID: i64 = 1_000_000;
/// The first exact-legacy ID is exactly representable by a JavaScript `Number`.
const FIRST_EXACT_LEGACY_SERVER_REQUEST_ID: i64 = -1;
/// The inclusive lower bound of JavaScript's integer-safe `Number` range.
const LAST_EXACT_LEGACY_SERVER_REQUEST_ID: i64 = -9_007_199_254_740_991;
const INVALID_LIMIT_ERROR: &str = "Invalid bidirectional request limit";
const IN_FLIGHT_LIMIT_ERROR: &str = "Bidirectional request limit reached";
const REQUEST_ID_EXHAUSTED_ERROR: &str = "Bidirectional request IDs exhausted";
const INVALID_RESPONSE_ERROR: &str = "Invalid JSON-RPC response";
const REMOTE_RESPONSE_ERROR: &str = "Client returned an error response";
const CONNECTION_CLOSED_ERROR: &str = "Bidirectional connection closed";
const TRANSPORT_SEND_ERROR: &str = "Failed to send bidirectional request";
const RESPONSE_CHANNEL_ERROR: &str = "Bidirectional response channel closed";
const RESPONSE_PAYLOAD_ERROR: &str = "Invalid bidirectional response payload";
const REQUEST_PAYLOAD_ERROR: &str = "Failed to serialize bidirectional request payload";
const INVALID_ELICITATION_REQUEST_ERROR: &str = "Invalid elicitation request";
const INVALID_MRTR_LIMIT_ERROR: &str = "Invalid MRTR exchange limit";
const MRTR_REQUEST_STATE_ERROR: &str = "Invalid or expired MRTR request state";
const MRTR_REQUEST_STATE_UNAVAILABLE_ERROR: &str = "Unable to create MRTR request state";
const MRTR_INPUT_MAP_ERROR: &str = "Invalid MRTR input request or response map";
const MRTR_RESPONSE_KIND_ERROR: &str = "MRTR input response does not match its request";
const MRTR_ROUND_LIMIT_ERROR: &str = "MRTR exchange limit reached";

/// The immutable request facts a router binds to one opaque MRTR state.
///
/// This is deliberately server-local: it is never serialized and prevents a
/// state minted for one modern operation from resuming another operation that
/// happens to request the same embedded input kinds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MrtrExchangeBinding {
    method: &'static str,
    target: String,
    arguments_digest: [u8; 32],
    session_partition: [u8; 32],
    principal_digest: Option<[u8; 32]>,
    verified_grants_digest: [u8; 32],
    is_stateless: bool,
}

impl MrtrExchangeBinding {
    /// Captures the router-admitted operation identity for a future retry.
    #[must_use]
    pub(crate) fn new(
        method: &'static str,
        target: String,
        arguments_digest: [u8; 32],
        session_partition: [u8; 32],
        principal_digest: Option<[u8; 32]>,
        verified_grants_digest: [u8; 32],
    ) -> Self {
        Self {
            method,
            target,
            arguments_digest,
            session_partition,
            principal_digest,
            verified_grants_digest,
            is_stateless: false,
        }
    }

    /// Captures the operation identity for an ephemeral stateless HTTP retry.
    #[must_use]
    pub(crate) fn stateless(
        method: &'static str,
        target: String,
        arguments_digest: [u8; 32],
        session_partition: [u8; 32],
        principal_digest: Option<[u8; 32]>,
        verified_grants_digest: [u8; 32],
    ) -> Self {
        Self {
            method,
            target,
            arguments_digest,
            session_partition,
            principal_digest,
            verified_grants_digest,
            is_stateless: true,
        }
    }

    #[must_use]
    pub(crate) fn is_stateless(&self) -> bool {
        self.is_stateless
    }
}
const LEGACY_INPUT_RETRY_ERROR: &str = "MCP 2024-11-05 does not support input retries";

// ============================================================================
// Pending Request Tracking
// ============================================================================

/// A bounded, single-use channel for receiving a response.
type PendingResponse = McpResult<serde_json::Value>;
type ResponseSender = oneshot::Sender<PendingResponse>;
type ResponseReceiver = oneshot::Receiver<PendingResponse>;

/// Immutable wire-ID domain assigned to one pending-request tracker.
///
/// Exact legacy reverse requests descend from
/// [`FIRST_EXACT_LEGACY_SERVER_REQUEST_ID`] through JavaScript's negative safe
/// integer range. A response from the already issued suffix of that range can
/// therefore be retired without retaining one tombstone per completed request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PendingIdDomain {
    Positive,
    ExactLegacyNegative,
}

impl PendingIdDomain {
    const fn first_id(self) -> i64 {
        match self {
            Self::Positive => FIRST_SERVER_REQUEST_ID,
            Self::ExactLegacyNegative => FIRST_EXACT_LEGACY_SERVER_REQUEST_ID,
        }
    }

    fn next_id_after(self, candidate: i64) -> Option<i64> {
        match self {
            Self::Positive => candidate.checked_add(1),
            Self::ExactLegacyNegative => candidate
                .checked_sub(1)
                .filter(|next| *next >= LAST_EXACT_LEGACY_SERVER_REQUEST_ID),
        }
    }

    fn is_issued_negative_suffix(self, next_id: Option<i64>, id: &CorrelationKey) -> bool {
        let Self::ExactLegacyNegative = self else {
            return false;
        };
        let CorrelationKey::Integer(integer) = id else {
            return false;
        };
        let Ok(id) = integer.parse::<i64>() else {
            return false;
        };

        match next_id {
            // `next_id` itself has not yet been issued, so the issued suffix
            // is open at its lower end: `(next_id..=-1)`.
            Some(next_id) => {
                (LAST_EXACT_LEGACY_SERVER_REQUEST_ID..=FIRST_EXACT_LEGACY_SERVER_REQUEST_ID)
                    .contains(&next_id)
                    && (next_id < id)
                    && (id <= FIRST_EXACT_LEGACY_SERVER_REQUEST_ID)
            }
            None => (LAST_EXACT_LEGACY_SERVER_REQUEST_ID..=FIRST_EXACT_LEGACY_SERVER_REQUEST_ID)
                .contains(&id),
        }
    }
}

/// Result of routing one response through [`PendingRequests`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingResponseDisposition {
    /// The response reached its live pending request.
    Delivered,
    /// The response belongs to an issued exact-legacy negative ID that has
    /// already left the pending set.
    RetiredGeneric,
    /// The response ID was not issued by this tracker or is absent.
    Unmatched,
}

#[derive(Debug)]
struct PendingState {
    requests: HashMap<CorrelationKey, PendingRequest>,
    next_id: Option<i64>,
    closed: bool,
}

#[derive(Debug)]
struct PendingRequest {
    sender: ResponseSender,
    request_cancellation: Option<McpRequestCancellation>,
}

/// Tracks pending server-to-client requests.
///
/// When the server sends a request to the client, it registers a response sender
/// here. When a response arrives, the dispatcher routes it to the correct sender.
#[derive(Debug)]
pub struct PendingRequests {
    state: Mutex<PendingState>,
    id_domain: PendingIdDomain,
    max_in_flight: usize,
}

impl PendingRequests {
    pub(crate) fn validate_max_in_flight(max_in_flight: usize) -> McpResult<()> {
        if !(1..=HARD_MAX_IN_FLIGHT_REQUESTS).contains(&max_in_flight) {
            return Err(McpError::new(
                McpErrorCode::InvalidParams,
                INVALID_LIMIT_ERROR,
            ));
        }
        Ok(())
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, PendingState> {
        match self.state.lock() {
            Ok(guard) => guard,
            // Prefer availability over panic if another task panicked while holding the lock.
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn new_in_domain(id_domain: PendingIdDomain, max_in_flight: usize) -> Self {
        Self {
            state: Mutex::new(PendingState {
                requests: HashMap::new(),
                next_id: Some(id_domain.first_id()),
                closed: false,
            }),
            id_domain,
            max_in_flight,
        }
    }

    /// Creates a new pending request tracker.
    #[must_use]
    pub fn new() -> Self {
        Self::new_in_domain(PendingIdDomain::Positive, DEFAULT_MAX_IN_FLIGHT_REQUESTS)
    }

    /// Creates a tracker with a caller-selected finite in-flight limit.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` when `max_in_flight` is zero or exceeds
    /// [`HARD_MAX_IN_FLIGHT_REQUESTS`].
    pub fn with_max_in_flight(max_in_flight: usize) -> McpResult<Self> {
        Self::validate_max_in_flight(max_in_flight)?;

        Ok(Self::new_in_domain(
            PendingIdDomain::Positive,
            max_in_flight,
        ))
    }

    /// Creates the exact-legacy tracker whose request IDs descend from
    /// [`FIRST_EXACT_LEGACY_SERVER_REQUEST_ID`] through the JavaScript-safe
    /// negative domain.
    ///
    /// The domain is fixed for the tracker's full lifetime so a response for
    /// an issued-but-retired negative ID can be classified in O(1) space.
    pub(crate) fn with_max_in_flight_for_exact_legacy(max_in_flight: usize) -> McpResult<Self> {
        Self::validate_max_in_flight(max_in_flight)?;

        Ok(Self::new_in_domain(
            PendingIdDomain::ExactLegacyNegative,
            max_in_flight,
        ))
    }

    /// Returns the configured maximum number of in-flight requests.
    #[must_use]
    pub const fn max_in_flight(&self) -> usize {
        self.max_in_flight
    }

    /// Returns the current number of in-flight requests.
    #[must_use]
    pub fn in_flight_len(&self) -> usize {
        self.lock_state().requests.len()
    }

    /// Atomically allocates a collision-free ID and registers its response
    /// channel. Allocation scans at most `max_in_flight + 1` candidates.
    fn register(&self) -> McpResult<(RequestId, ResponseReceiver)> {
        self.register_with_cancellation(None)
    }

    fn register_with_cancellation(
        &self,
        request_cancellation: Option<McpRequestCancellation>,
    ) -> McpResult<(RequestId, ResponseReceiver)> {
        let mut state = self.lock_state();
        if state.closed {
            return Err(McpError::internal_error(CONNECTION_CLOSED_ERROR));
        }
        if state.requests.len() >= self.max_in_flight {
            return Err(McpError::internal_error(IN_FLIGHT_LIMIT_ERROR));
        }

        for _ in 0..=self.max_in_flight {
            let Some(candidate) = state.next_id else {
                return Err(McpError::internal_error(REQUEST_ID_EXHAUSTED_ERROR));
            };
            let id = RequestId::Number(candidate);
            let key = id
                .correlation_key()
                .map_err(|_| McpError::internal_error(REQUEST_ID_EXHAUSTED_ERROR))?;
            state.next_id = self.id_domain.next_id_after(candidate);

            if let Entry::Vacant(entry) = state.requests.entry(key) {
                let (sender, receiver) = oneshot::channel();
                entry.insert(PendingRequest {
                    sender,
                    request_cancellation,
                });
                return Ok((id, receiver));
            }
        }

        Err(McpError::internal_error(REQUEST_ID_EXHAUSTED_ERROR))
    }

    /// Routes a response to the appropriate pending request.
    ///
    /// Returns `true` only when the response was delivered to a live pending
    /// request, preserving the established public boolean contract.
    pub fn route_response(&self, response: &JsonRpcResponse) -> bool {
        matches!(
            self.route_response_with_disposition(response),
            PendingResponseDisposition::Delivered
        )
    }

    /// Routes a response and reports whether it was delivered, retired from
    /// the exact-legacy negative ID suffix, or unmatched.
    pub(crate) fn route_response_with_disposition(
        &self,
        response: &JsonRpcResponse,
    ) -> PendingResponseDisposition {
        let Some(ref id) = response.id else {
            return PendingResponseDisposition::Unmatched;
        };
        let Ok(key) = id.correlation_key() else {
            return PendingResponseDisposition::Unmatched;
        };

        let (pending, retired_generic) = {
            let mut state = self.lock_state();
            let pending = state.requests.remove(&key);
            let retired_generic = pending.is_none()
                && self
                    .id_domain
                    .is_issued_negative_suffix(state.next_id, &key);
            (pending, retired_generic)
        };

        if let Some(pending) = pending {
            // Validate every response invariant before consuming the waiter.
            // This also rejects manually-constructed values that bypass serde's guards.
            let validated = ValidatedResponse::from_response(response);
            let outcome = validated.into_pending_response();
            // The response path is synchronous, so use the immediate bounded
            // oneshot bridge. Receiver dropout returns the value and is safe to
            // ignore after the map entry has been removed.
            let _ = pending.sender.send_blocking(outcome);
            PendingResponseDisposition::Delivered
        } else if retired_generic {
            PendingResponseDisposition::RetiredGeneric
        } else {
            PendingResponseDisposition::Unmatched
        }
    }

    /// Removes a pending request (e.g., on timeout or cancellation).
    pub fn remove(&self, id: &RequestId) {
        let Ok(key) = id.correlation_key() else {
            return;
        };
        let mut state = self.lock_state();
        state.requests.remove(&key);
    }

    /// Wakes pending server-to-client calls whose owning incoming request is
    /// terminal, without mutating the caller-owned connection context.
    pub(crate) fn cancel_cancelled(&self) -> usize {
        let cancelled = {
            let mut state = self.lock_state();
            let ids: Vec<CorrelationKey> = state
                .requests
                .iter()
                .filter(|(_, pending)| {
                    pending
                        .request_cancellation
                        .as_ref()
                        .is_some_and(McpRequestCancellation::is_terminal)
                })
                .map(|(id, _)| id.clone())
                .collect();
            ids.into_iter()
                .filter_map(|id| state.requests.remove(&id))
                .collect::<Vec<_>>()
        };
        let count = cancelled.len();
        for pending in cancelled {
            let _ = pending
                .sender
                .send_blocking(Err(McpError::request_cancelled()));
        }
        count
    }

    /// Permanently closes the tracker and cancels every pending request.
    ///
    /// Closing is irreversible: later registration attempts fail with the
    /// same fixed connection-closed error. This prevents a request racing with
    /// connection teardown from installing an orphaned waiter after the drain.
    pub fn cancel_all(&self) {
        let senders: Vec<PendingRequest> = {
            let mut state = self.lock_state();
            state.closed = true;
            state.requests.drain().map(|(_, pending)| pending).collect()
        };
        for pending in senders {
            let _ = pending
                .sender
                .send_blocking(Err(McpError::internal_error(CONNECTION_CLOSED_ERROR)));
        }
    }

    #[cfg(test)]
    fn set_next_id_for_test(&self, next_id: i64) {
        self.lock_state().next_id = Some(next_id);
    }
}

enum ValidatedResponse<'a> {
    Success(&'a serde_json::Value),
    Error(&'a JsonRpcError),
    Invalid,
}

impl<'a> ValidatedResponse<'a> {
    fn from_response(response: &'a JsonRpcResponse) -> Self {
        if response.validate().is_err() {
            return Self::Invalid;
        }

        match (&response.result, &response.error) {
            (Some(result), None) => Self::Success(result),
            (None, Some(error)) => Self::Error(error),
            (Some(_), Some(_)) | (None, None) => Self::Invalid,
        }
    }

    fn into_pending_response(self) -> PendingResponse {
        match self {
            Self::Success(result) => Ok(result.clone()),
            Self::Error(error) => Err(McpError::new(
                error
                    .code
                    .as_i32()
                    .map(McpErrorCode::from)
                    .unwrap_or(McpErrorCode::InternalError),
                REMOTE_RESPONSE_ERROR,
            )),
            Self::Invalid => Err(McpError::internal_error(INVALID_RESPONSE_ERROR)),
        }
    }
}

impl Default for PendingRequests {
    fn default() -> Self {
        Self::new()
    }
}

/// Owns the local and peer-facing cleanup for one reverse request.
///
/// Once the outbound request has been committed to the transport, dropping its
/// future without a routed response must both free the local slot and tell the
/// exact-2024 peer to stop work. MCP 2026-07-28 does not permit this reverse
/// cancellation control. The legacy notification remains best effort because
/// a closing transport cannot reliably deliver another frame.
struct PendingRequestGuard {
    pending: Arc<PendingRequests>,
    send_fn: TransportSendFn,
    era: ProtocolEra,
    id: RequestId,
    request_sent: bool,
    finished: bool,
}

impl PendingRequestGuard {
    fn mark_request_sent(&mut self) {
        self.request_sent = true;
    }

    fn finish(&mut self) {
        self.finished = true;
        self.pending.remove(&self.id);
    }

    fn cancel(&mut self) {
        self.pending.remove(&self.id);
        self.send_cancellation_notification();
        self.finished = true;
    }

    fn send_cancellation_notification(&self) {
        if !self.request_sent || self.era != ProtocolEra::Legacy2024 {
            return;
        }

        let message = JsonRpcMessage::Request(JsonRpcRequest::notification(
            "notifications/cancelled",
            Some(serde_json::json!({ "requestId": self.id.clone() })),
        ));
        // A reverse request is already terminal locally. Do not replace that
        // outcome with a best-effort control-frame transport failure.
        let _ = (self.send_fn)(&message);
    }
}

impl Drop for PendingRequestGuard {
    fn drop(&mut self) {
        self.pending.remove(&self.id);
        if !self.finished {
            self.send_cancellation_notification();
        }
    }
}

// ============================================================================
// Transport Request Sender
// ============================================================================

/// Callback type for sending messages through the transport.
pub type TransportSendFn = Arc<dyn Fn(&JsonRpcMessage) -> Result<(), String> + Send + Sync>;

/// Sends server-to-client requests through the transport.
///
/// This struct provides a way to send requests to the client and await responses.
/// It works in conjunction with [`PendingRequests`] to track in-flight requests.
#[derive(Clone)]
pub struct RequestSender {
    /// Pending request tracker.
    pending: Arc<PendingRequests>,
    /// Transport send callback.
    send_fn: TransportSendFn,
    /// Exact protocol era that governs reverse-request cleanup controls.
    era: ProtocolEra,
    /// Request-local cancellation domain installed by server dispatch.
    request_cancellation: Option<McpRequestCancellation>,
}

impl RequestSender {
    /// Creates an exact MCP 2024-11-05 request sender.
    ///
    /// Use [`Self::new_for_era`] when the negotiated era is available.
    pub fn new(pending: Arc<PendingRequests>, send_fn: TransportSendFn) -> Self {
        Self::new_for_era(ProtocolEra::Legacy2024, pending, send_fn)
    }

    /// Creates a request sender bound to one negotiated protocol era.
    ///
    /// Dropped reverse requests emit `notifications/cancelled` only for exact
    /// MCP 2024-11-05. MCP 2026-07-28 retains local cleanup but emits no
    /// server cancellation notification.
    pub fn new_for_era(
        era: ProtocolEra,
        pending: Arc<PendingRequests>,
        send_fn: TransportSendFn,
    ) -> Self {
        Self {
            pending,
            send_fn,
            era,
            request_cancellation: None,
        }
    }

    pub(crate) fn for_request(&self, request_cancellation: McpRequestCancellation) -> Self {
        Self {
            pending: Arc::clone(&self.pending),
            send_fn: Arc::clone(&self.send_fn),
            era: self.era,
            request_cancellation: Some(request_cancellation),
        }
    }

    fn request_is_terminal(&self) -> bool {
        self.request_cancellation
            .as_ref()
            .is_some_and(McpRequestCancellation::is_terminal)
    }

    /// Sends a request to the client and waits for a response.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - The finite in-flight request limit is reached
    /// - The transport send fails
    /// - The request times out (based on budget)
    /// - The client returns an error response
    /// - The response envelope or typed payload is invalid
    /// - The connection is closed
    pub async fn send_request<T: serde::de::DeserializeOwned>(
        &self,
        cx: &Cx,
        method: &str,
        params: serde_json::Value,
    ) -> McpResult<T> {
        if cx.checkpoint().is_err() || self.request_is_terminal() {
            return Err(McpError::request_cancelled());
        }

        let (id, mut receiver) = self
            .pending
            .register_with_cancellation(self.request_cancellation.clone())?;
        let mut guard = PendingRequestGuard {
            pending: Arc::clone(&self.pending),
            send_fn: Arc::clone(&self.send_fn),
            era: self.era,
            id: id.clone(),
            request_sent: false,
            finished: false,
        };
        if cx.checkpoint().is_err() || self.request_is_terminal() {
            return Err(McpError::request_cancelled());
        }

        let request = JsonRpcRequest::new(method.to_string(), Some(params), id.clone());
        let message = JsonRpcMessage::Request(request);

        // Send the request through the transport
        if (self.send_fn)(&message).is_err() {
            return Err(McpError::internal_error(TRANSPORT_SEND_ERROR));
        }
        guard.mark_request_sent();

        let response = if let Some(request_cancellation) = &self.request_cancellation {
            let mut receive = std::pin::pin!(receiver.recv(cx));
            let mut terminated = std::pin::pin!(request_cancellation.terminated());

            poll_fn(|task_cx| {
                // Request termination owns ties: check before polling either
                // source, after arming its waiter, and once more after polling
                // the response future.
                if request_cancellation.is_terminal() {
                    return Poll::Ready(Err(McpError::request_cancelled()));
                }
                if terminated.as_mut().poll(task_cx).is_ready() {
                    return Poll::Ready(Err(McpError::request_cancelled()));
                }

                let receive_poll = receive.as_mut().poll(task_cx);
                if request_cancellation.is_terminal() {
                    return Poll::Ready(Err(McpError::request_cancelled()));
                }
                match receive_poll {
                    Poll::Ready(Ok(response)) => Poll::Ready(response),
                    Poll::Ready(Err(RecvError::Cancelled)) => {
                        Poll::Ready(Err(McpError::request_cancelled()))
                    }
                    Poll::Ready(Err(RecvError::Closed | RecvError::PolledAfterCompletion)) => {
                        Poll::Ready(Err(McpError::internal_error(RESPONSE_CHANNEL_ERROR)))
                    }
                    Poll::Pending => Poll::Pending,
                }
            })
            .await
        } else {
            match receiver.recv(cx).await {
                Ok(response) => response,
                Err(RecvError::Cancelled) => Err(McpError::request_cancelled()),
                Err(RecvError::Closed | RecvError::PolledAfterCompletion) => {
                    Err(McpError::internal_error(RESPONSE_CHANNEL_ERROR))
                }
            }
        };

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                if error.code == McpErrorCode::RequestCancelled
                    && (cx.checkpoint().is_err() || self.request_is_terminal())
                {
                    guard.cancel();
                } else {
                    guard.finish();
                }
                return Err(error);
            }
        };

        // A response and cancellation may become visible together. Preserve
        // caller cancellation/budget precedence before decoding peer data.
        if cx.checkpoint().is_err() || self.request_is_terminal() {
            guard.cancel();
            return Err(McpError::request_cancelled());
        }

        let result = serde_json::from_value(response)
            .map_err(|_| McpError::internal_error(RESPONSE_PAYLOAD_ERROR));
        guard.finish();
        result
    }
}

impl std::fmt::Debug for RequestSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestSender")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

// ============================================================================
// Sampling Sender Implementation
// ============================================================================

/// Sends sampling requests to the client via the transport.
#[derive(Clone)]
pub struct TransportSamplingSender {
    sender: RequestSender,
    request_context: McpContext,
}

impl TransportSamplingSender {
    /// Creates a sampling sender bound to the originating handler request.
    pub fn new(sender: RequestSender, request_context: McpContext) -> Self {
        Self {
            sender,
            request_context,
        }
    }
}

impl SamplingSender for TransportSamplingSender {
    fn create_message(
        &self,
        request: SamplingRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = McpResult<SamplingResponse>> + Send + '_>>
    {
        Box::pin(async move {
            // Convert to protocol types
            let params = fastmcp_protocol::CreateMessageParams {
                messages: request
                    .messages
                    .into_iter()
                    .map(|m| fastmcp_protocol::SamplingMessage {
                        role: match m.role {
                            SamplingRole::User => fastmcp_protocol::Role::User,
                            SamplingRole::Assistant => fastmcp_protocol::Role::Assistant,
                        },
                        content: fastmcp_protocol::SamplingContent::Text { text: m.text },
                    })
                    .collect(),
                max_tokens: fastmcp_protocol::JsonInteger::from(u64::from(request.max_tokens)),
                system_prompt: request.system_prompt,
                temperature: request.temperature,
                stop_sequences: request.stop_sequences,
                model_preferences: if request.model_hints.is_empty() {
                    None
                } else {
                    Some(fastmcp_protocol::ModelPreferences {
                        hints: request
                            .model_hints
                            .into_iter()
                            .map(|name| fastmcp_protocol::ModelHint { name: Some(name) })
                            .collect(),
                        ..Default::default()
                    })
                },
                include_context: None,
                metadata: None,
                meta: None,
            };

            let params_value = serde_json::to_value(&params)
                .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?;

            self.request_context
                .checkpoint()
                .map_err(|_| McpError::request_cancelled())?;
            let result: fastmcp_protocol::CreateMessageResult = self
                .sender
                .send_request(
                    self.request_context.cx(),
                    "sampling/createMessage",
                    params_value,
                )
                .await?;

            if result.role != fastmcp_protocol::Role::Assistant {
                return Err(McpError::internal_error(RESPONSE_PAYLOAD_ERROR));
            }

            Ok(SamplingResponse {
                text: match result.content {
                    fastmcp_protocol::SamplingContent::Text { text } => text,
                    fastmcp_protocol::SamplingContent::Image { data, mime_type } => {
                        format!("[image: {} bytes, type: {}]", data.len(), mime_type)
                    }
                },
                model: result.model,
                stop_reason: SamplingStopReason::from_wire_value(result.stop_reason),
            })
        })
    }
}

// ============================================================================
// Elicitation Sender Implementation
// ============================================================================

/// Sends elicitation requests to the client via the transport.
#[derive(Clone)]
pub struct TransportElicitationSender {
    sender: RequestSender,
    request_context: McpContext,
}

impl TransportElicitationSender {
    /// Creates an elicitation sender bound to the originating handler request.
    pub fn new(sender: RequestSender, request_context: McpContext) -> Self {
        Self {
            sender,
            request_context,
        }
    }
}

impl ElicitationSender for TransportElicitationSender {
    fn elicit(
        &self,
        request: ElicitationRequest,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = McpResult<ElicitationResponse>> + Send + '_>,
    > {
        Box::pin(async move {
            let request_mode = request.mode;
            let params_value = match request_mode {
                ElicitationMode::Form => {
                    let requested_schema = request.schema.ok_or_else(|| {
                        McpError::invalid_params(INVALID_ELICITATION_REQUEST_ERROR)
                    })?;
                    let params = fastmcp_protocol::ElicitRequestFormParams {
                        mode: fastmcp_protocol::ElicitMode::Form,
                        message: request.message.clone(),
                        requested_schema,
                    };
                    serde_json::to_value(&params)
                        .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?
                }
                ElicitationMode::Url => {
                    let url = request
                        .url
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                            McpError::invalid_params(INVALID_ELICITATION_REQUEST_ERROR)
                        })?;
                    let elicitation_id = request
                        .elicitation_id
                        .filter(|value| !value.is_empty())
                        .ok_or_else(|| {
                        McpError::invalid_params(INVALID_ELICITATION_REQUEST_ERROR)
                    })?;
                    let params = fastmcp_protocol::ElicitRequestUrlParams {
                        mode: fastmcp_protocol::ElicitMode::Url,
                        message: request.message.clone(),
                        url,
                        elicitation_id,
                    };
                    serde_json::to_value(&params)
                        .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?
                }
            };

            self.request_context
                .checkpoint()
                .map_err(|_| McpError::request_cancelled())?;
            let result: fastmcp_protocol::ElicitResult = self
                .sender
                .send_request(
                    self.request_context.cx(),
                    "elicitation/create",
                    params_value,
                )
                .await?;

            let action = match result.action {
                fastmcp_protocol::ElicitAction::Accept => ElicitationAction::Accept,
                fastmcp_protocol::ElicitAction::Decline => ElicitationAction::Decline,
                fastmcp_protocol::ElicitAction::Cancel => ElicitationAction::Cancel,
            };

            // Decline/cancel content is not accepted form data. It remains a
            // wire-level SHOULD deviation in the full protocol design, but
            // this legacy core response has no quarantine slot, so discard it
            // instead of exposing it to business logic. Accepted URL mode must
            // never carry in-band form content.
            let content = match (request_mode, result.action, result.content) {
                (ElicitationMode::Form, fastmcp_protocol::ElicitAction::Accept, Some(content)) => {
                    content
                }
                (ElicitationMode::Form, fastmcp_protocol::ElicitAction::Accept, None)
                | (ElicitationMode::Url, fastmcp_protocol::ElicitAction::Accept, Some(_)) => {
                    return Err(McpError::internal_error(RESPONSE_PAYLOAD_ERROR));
                }
                (
                    _,
                    fastmcp_protocol::ElicitAction::Decline
                    | fastmcp_protocol::ElicitAction::Cancel,
                    _,
                )
                | (ElicitationMode::Url, fastmcp_protocol::ElicitAction::Accept, None) => {
                    return Ok(ElicitationResponse {
                        action,
                        content: None,
                    });
                }
            };

            // Convert HashMap<String, ElicitContentValue> to HashMap<String, serde_json::Value>.
            let content = {
                let mut map = std::collections::HashMap::new();
                for (key, value) in content {
                    let json_value = match value {
                        fastmcp_protocol::ElicitContentValue::Null => serde_json::Value::Null,
                        fastmcp_protocol::ElicitContentValue::Bool(b) => serde_json::Value::Bool(b),
                        fastmcp_protocol::ElicitContentValue::Int(i) => serde_json::to_value(i)
                            .map_err(|_| McpError::internal_error(RESPONSE_PAYLOAD_ERROR))?,
                        fastmcp_protocol::ElicitContentValue::Float(f) => {
                            serde_json::Number::from_f64(f)
                                .map(serde_json::Value::Number)
                                .unwrap_or(serde_json::Value::Null)
                        }
                        fastmcp_protocol::ElicitContentValue::String(s) => {
                            serde_json::Value::String(s)
                        }
                        fastmcp_protocol::ElicitContentValue::StringArray(arr) => {
                            serde_json::Value::Array(
                                arr.into_iter().map(serde_json::Value::String).collect(),
                            )
                        }
                    };
                    map.insert(key, json_value);
                }
                Some(map)
            };

            Ok(ElicitationResponse { action, content })
        })
    }
}

// ============================================================================
// Roots Provider Implementation
// ============================================================================

/// Provider for filesystem roots from the client.
#[derive(Clone)]
pub struct TransportRootsProvider {
    sender: RequestSender,
    request_context: McpContext,
}

impl TransportRootsProvider {
    /// Creates a roots provider bound to the originating handler request.
    ///
    /// The provider retains the full framework context, rather than its raw
    /// `Cx`, so its reverse `roots/list` request observes the originating
    /// request lease, framework budget ceiling, and cancellation domain.
    pub fn new(sender: RequestSender, request_context: McpContext) -> Self {
        Self {
            sender,
            request_context,
        }
    }

    /// Lists the filesystem roots from the client.
    pub async fn list_roots(&self) -> McpResult<Vec<fastmcp_protocol::Root>> {
        self.request_context
            .checkpoint()
            .map_err(|_| McpError::request_cancelled())?;
        let request = self.sender.send_request(
            self.request_context.cx(),
            "roots/list",
            serde_json::json!({}),
        );
        let result: fastmcp_protocol::ListRootsResult = match self.request_context.budget().deadline
        {
            Some(deadline) => asupersync::time::timeout_at(deadline, request)
                .await
                .map_err(|_| McpError::request_cancelled())??,
            None => request.await?,
        };
        self.request_context
            .ensure_live()
            .map_err(|_| McpError::request_cancelled())?;
        Ok(result.roots)
    }
}

impl RootsProvider for TransportRootsProvider {
    fn list_roots(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = McpResult<Vec<ClientRoot>>> + Send + '_>>
    {
        Box::pin(async move {
            let roots = TransportRootsProvider::list_roots(self).await?;
            Ok(roots
                .into_iter()
                .map(|root| ClientRoot {
                    uri: root.uri,
                    name: root.name,
                })
                .collect())
        })
    }
}

// ============================================================================
// Final MRTR Embedded Input Exchanges
// ============================================================================

/// The three server-to-client input kinds represented by final MRTR.
///
/// These are embedded descriptors inside an `inputRequests` map. They are not
/// independent JSON-RPC requests and never receive a JSON-RPC ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MrtrInputKind {
    /// `elicitation/create` with an [`fastmcp_protocol::ElicitResult`] response.
    Elicitation,
    /// `sampling/createMessage` with a [`fastmcp_protocol::FinalCreateMessageResult`] response.
    Sampling,
    /// `roots/list` with a [`fastmcp_protocol::ListRootsResult`] response.
    Roots,
}

impl MrtrInputKind {
    /// Returns the exact embedded input request method.
    #[must_use]
    pub const fn method(self) -> &'static str {
        match self {
            Self::Elicitation => "elicitation/create",
            Self::Sampling => "sampling/createMessage",
            Self::Roots => "roots/list",
        }
    }
}

/// Elicitation parameters selected by the negotiated protocol era.
///
/// Final embedded descriptors and exact-2024 JSON-RPC parameters deliberately
/// remain separate: the former has no legacy `elicitationId`, while the latter
/// requires one.
#[derive(Debug, Clone)]
pub enum DualEraElicitationParams {
    /// Exact-2024 reverse JSON-RPC elicitation parameters.
    Legacy2024(fastmcp_protocol::ElicitRequestParams),
    /// Final embedded MRTR elicitation parameters.
    Modern2026(fastmcp_protocol::FinalEmbeddedElicitationParams),
}

/// Sampling parameters selected by the negotiated protocol era.
///
/// Exact-2024 reverse JSON-RPC retains its legacy request model. Final MRTR
/// descriptors use the distinct embedded final model so tool declarations,
/// tool-choice controls, and final sampling content cannot be lost at the
/// era boundary.
#[derive(Debug, Clone)]
pub enum DualEraSamplingParams {
    /// Exact-2024 reverse JSON-RPC sampling parameters.
    Legacy2024(fastmcp_protocol::CreateMessageParams),
    /// Final embedded MRTR sampling parameters.
    Modern2026(fastmcp_protocol::FinalEmbeddedCreateMessageParams),
}

#[derive(Debug, Clone)]
enum MrtrInputParams {
    LegacyElicitation(fastmcp_protocol::ElicitRequestParams),
    FinalElicitation(fastmcp_protocol::FinalEmbeddedElicitationParams),
    FinalSampling(fastmcp_protocol::FinalEmbeddedCreateMessageParams),
    Json(serde_json::Value),
}

/// One final MRTR embedded input descriptor.
///
/// Its wire representation is exactly `{ "method": ..., "params": ... }`
/// when it has parameters, or `{ "method": "roots/list" }` for roots. It
/// deliberately has no JSON-RPC envelope, correlation ID, or inherited outer
/// request metadata.
#[derive(Debug, Clone)]
pub struct MrtrInputRequest {
    kind: MrtrInputKind,
    params: Option<MrtrInputParams>,
}

impl MrtrInputRequest {
    /// Creates an exact-2024 form or URL elicitation request.
    ///
    #[must_use]
    pub fn legacy_elicitation(params: fastmcp_protocol::ElicitRequestParams) -> Self {
        Self {
            kind: MrtrInputKind::Elicitation,
            params: Some(MrtrInputParams::LegacyElicitation(params)),
        }
    }

    /// Creates one final-era elicitation descriptor without translating it
    /// through the exact-2024 request shape.
    fn final_elicitation(
        params: fastmcp_protocol::FinalEmbeddedElicitationParams,
    ) -> McpResult<Self> {
        serde_json::to_value(&params)
            .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?;
        Ok(Self {
            kind: MrtrInputKind::Elicitation,
            params: Some(MrtrInputParams::FinalElicitation(params)),
        })
    }

    /// Creates a final sampling input descriptor.
    ///
    /// The embedded descriptor must not inherit the outer request's metadata,
    /// so this safe constructor always omits `_meta`.
    ///
    /// # Errors
    ///
    /// Returns an internal error only if its protocol parameters cannot be
    /// represented as JSON.
    pub fn sampling(params: fastmcp_protocol::FinalEmbeddedCreateMessageParams) -> McpResult<Self> {
        serde_json::to_value(&params)
            .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?;
        Ok(Self {
            kind: MrtrInputKind::Sampling,
            params: Some(MrtrInputParams::FinalSampling(params)),
        })
    }

    /// Creates a final roots input descriptor with omitted parameters.
    #[must_use]
    pub const fn roots() -> Self {
        Self {
            kind: MrtrInputKind::Roots,
            params: None,
        }
    }

    /// Returns the input's exact response kind.
    #[must_use]
    pub const fn kind(&self) -> MrtrInputKind {
        self.kind
    }

    /// Checks accepted final elicitation data against the exact descriptor
    /// retained at issuance. A client-side form resolver is not a trust
    /// boundary for server handlers receiving an arbitrary peer's retry.
    fn validate_response(&self, response: &MrtrInputResponse) -> McpResult<()> {
        let Some(MrtrInputParams::FinalElicitation(params)) = &self.params else {
            return Ok(());
        };
        if response.value.get("action").and_then(serde_json::Value::as_str) != Some("accept") {
            // Decline/cancel content is a schema-valid peer SHOULD deviation.
            // It is not accepted form data and must not be checked against the
            // requested schema.
            return Ok(());
        }
        match params {
            fastmcp_protocol::FinalEmbeddedElicitationParams::Form(params) => {
                let content = response
                    .value
                    .get("content")
                    .filter(|content| content.is_object())
                    .ok_or_else(|| McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR))?;
                params
                    .requested_schema
                    .validate(content)
                    .map_err(|_| McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR))
            }
            fastmcp_protocol::FinalEmbeddedElicitationParams::Url(_) => {
                if response.value.get("content").is_some() {
                    return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
                }
                Ok(())
            }
        }
    }

    /// Decodes one handler-declared embedded input descriptor.
    ///
    /// Only the three final MRTR methods are admitted. In particular, a
    /// handler cannot smuggle an arbitrary JSON-RPC request or outer metadata
    /// through the framework-minted input-required result.
    pub(crate) fn from_wire(value: &serde_json::Value) -> McpResult<Self> {
        let Some(object) = value.as_object() else {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        };
        if object.len() > 2 || object.keys().any(|key| key != "method" && key != "params") {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }
        let Some(method) = object.get("method").and_then(serde_json::Value::as_str) else {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        };
        let params = object.get("params");
        match method {
            "elicitation/create" => {
                let params =
                    params.ok_or_else(|| McpError::invalid_params(MRTR_INPUT_MAP_ERROR))?;
                Self::final_elicitation(
                    serde_json::from_value(params.clone())
                        .map_err(|_| McpError::invalid_params(MRTR_INPUT_MAP_ERROR))?,
                )
            }
            "sampling/createMessage" => {
                let params =
                    params.ok_or_else(|| McpError::invalid_params(MRTR_INPUT_MAP_ERROR))?;
                Self::sampling(
                    serde_json::from_value(params.clone())
                        .map_err(|_| McpError::invalid_params(MRTR_INPUT_MAP_ERROR))?,
                )
            }
            "roots/list" if params.is_none() => Ok(Self::roots()),
            _ => Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR)),
        }
    }

    fn with_params<T: Serialize>(kind: MrtrInputKind, params: T) -> McpResult<Self> {
        let params = serde_json::to_value(params)
            .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR))?;
        Ok(Self {
            kind,
            params: Some(MrtrInputParams::Json(params)),
        })
    }

    fn into_legacy_params(self) -> McpResult<serde_json::Value> {
        match self.params {
            None => Ok(serde_json::json!({})),
            Some(MrtrInputParams::LegacyElicitation(params)) => serde_json::to_value(params)
                .map_err(|_| McpError::internal_error(REQUEST_PAYLOAD_ERROR)),
            Some(MrtrInputParams::FinalElicitation(_)) => {
                Err(McpError::invalid_params(INVALID_ELICITATION_REQUEST_ERROR))
            }
            Some(MrtrInputParams::FinalSampling(_)) => Err(McpError::invalid_params(
                "Final sampling cannot be sent as exact-2024 reverse JSON-RPC",
            )),
            Some(MrtrInputParams::Json(params)) => Ok(params),
        }
    }
}

impl Serialize for MrtrInputRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut descriptor = serializer.serialize_struct(
            "MrtrInputRequest",
            if self.params.is_some() { 2 } else { 1 },
        )?;
        descriptor.serialize_field("method", self.kind.method())?;
        if let Some(params) = &self.params {
            match params {
                MrtrInputParams::LegacyElicitation(params) => {
                    descriptor.serialize_field("params", params)?;
                }
                MrtrInputParams::FinalElicitation(params) => {
                    descriptor.serialize_field("params", params)?;
                }
                MrtrInputParams::FinalSampling(params) => {
                    descriptor.serialize_field("params", params)?;
                }
                MrtrInputParams::Json(params) => {
                    descriptor.serialize_field("params", params)?;
                }
            }
        }
        descriptor.end()
    }
}

/// A typed input response whose value can be accepted only for the matching
/// [`MrtrInputKind`] recorded at issuance time.
#[derive(Debug, Clone)]
pub struct MrtrInputResponse {
    kind: MrtrInputKind,
    value: serde_json::Value,
}

impl MrtrInputResponse {
    /// Creates an elicitation response value.
    ///
    /// # Errors
    ///
    /// Returns an internal error only if its protocol value cannot be
    /// represented as JSON.
    pub fn elicitation(value: fastmcp_protocol::ElicitResult) -> McpResult<Self> {
        Self::with_value(MrtrInputKind::Elicitation, value)
    }

    /// Creates a sampling response value.
    ///
    /// # Errors
    ///
    /// Returns an internal error only if its protocol value cannot be
    /// represented as JSON.
    pub fn sampling(value: fastmcp_protocol::FinalCreateMessageResult) -> McpResult<Self> {
        Self::with_value(MrtrInputKind::Sampling, value)
    }

    /// Creates a roots response value.
    ///
    /// # Errors
    ///
    /// Returns an internal error only if its protocol value cannot be
    /// represented as JSON.
    pub fn roots(value: fastmcp_protocol::ListRootsResult) -> McpResult<Self> {
        Self::with_value(MrtrInputKind::Roots, value)
    }

    /// Returns the response's exact kind.
    #[must_use]
    pub const fn kind(&self) -> MrtrInputKind {
        self.kind
    }

    /// Returns this value as the elicitation result it was admitted as.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` when the caller asks for the wrong response
    /// kind. This is a handler-facing resume surface, not a wire decoder.
    pub fn elicitation_result(&self) -> McpResult<fastmcp_protocol::ElicitResult> {
        if self.kind != MrtrInputKind::Elicitation {
            return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
        }
        serde_json::from_value(self.value.clone())
            .map_err(|_| McpError::internal_error(MRTR_RESPONSE_KIND_ERROR))
    }

    /// Returns this value as the sampling result it was admitted as.
    pub fn sampling_result(&self) -> McpResult<fastmcp_protocol::FinalCreateMessageResult> {
        if self.kind != MrtrInputKind::Sampling {
            return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
        }
        serde_json::from_value(self.value.clone())
            .map_err(|_| McpError::internal_error(MRTR_RESPONSE_KIND_ERROR))
    }

    /// Returns this value as the roots result it was admitted as.
    pub fn roots_result(&self) -> McpResult<fastmcp_protocol::ListRootsResult> {
        if self.kind != MrtrInputKind::Roots {
            return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
        }
        serde_json::from_value(self.value.clone())
            .map_err(|_| McpError::internal_error(MRTR_RESPONSE_KIND_ERROR))
    }

    fn from_wire(kind: MrtrInputKind, value: serde_json::Value) -> McpResult<Self> {
        if kind == MrtrInputKind::Elicitation
            && value.get("content").is_some_and(|content| !content.is_object())
        {
            // Preserve absent versus explicitly null before the legacy-shaped
            // handler value's Option decoder can collapse those wire states.
            return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
        }
        let response = match kind {
            MrtrInputKind::Elicitation => Self::elicitation(
                serde_json::from_value(value)
                    .map_err(|_| McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR))?,
            )?,
            MrtrInputKind::Sampling => Self::sampling(
                serde_json::from_value(value)
                    .map_err(|_| McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR))?,
            )?,
            MrtrInputKind::Roots => Self::roots(
                serde_json::from_value(value)
                    .map_err(|_| McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR))?,
            )?,
        };
        Ok(response)
    }

    fn with_value<T: Serialize>(kind: MrtrInputKind, value: T) -> McpResult<Self> {
        let value = serde_json::to_value(value)
            .map_err(|_| McpError::internal_error(RESPONSE_PAYLOAD_ERROR))?;
        Ok(Self { kind, value })
    }
}

impl Serialize for MrtrInputResponse {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.value.serialize(serializer)
    }
}

/// A unique, bounded map of final embedded MRTR input requests.
#[derive(Debug, Clone, Default)]
pub struct MrtrInputRequests {
    entries: BTreeMap<String, MrtrInputRequest>,
}

impl MrtrInputRequests {
    /// Creates a unique map of embedded input request descriptors.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` for an empty or duplicate key, or when more
    /// than [`HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND`] descriptors are given.
    pub fn new(entries: impl IntoIterator<Item = (String, MrtrInputRequest)>) -> McpResult<Self> {
        let mut result = Self::default();
        for (key, request) in entries {
            result.insert(key, request)?;
        }
        Ok(result)
    }

    /// Returns whether this map contains no embedded input descriptors.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of embedded input descriptors.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Looks up one embedded input descriptor by its server-issued key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&MrtrInputRequest> {
        self.entries.get(key)
    }

    /// Iterates over server-issued input keys and their embedded descriptors.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &MrtrInputRequest)> {
        self.entries
            .iter()
            .map(|(key, request)| (key.as_str(), request))
    }

    fn insert(&mut self, key: String, request: MrtrInputRequest) -> McpResult<()> {
        if key.is_empty()
            || self.entries.len() >= HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND
            || self.entries.contains_key(&key)
        {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }
        self.entries.insert(key, request);
        Ok(())
    }

    fn unresolved_after(&self, responses: &MrtrInputResponses) -> Self {
        Self {
            entries: self
                .entries
                .iter()
                .filter(|(key, _)| responses.get(key).is_none())
                .map(|(key, request)| (key.clone(), request.clone()))
                .collect(),
        }
    }
}

impl Serialize for MrtrInputRequests {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.entries.serialize(serializer)
    }
}

/// An ordered, unique, bounded collection of final MRTR input response values.
///
/// The vector preserves the exact accepted retry order all the way through
/// [`MrtrCompletedInputs`]. A separate lookup index supports handler key
/// access without normalizing the handler-visible response sequence into a
/// sorted map.
#[derive(Debug, Clone, Default)]
pub struct MrtrInputResponses {
    entries: Vec<(String, MrtrInputResponse)>,
    index: HashMap<String, usize>,
}

impl MrtrInputResponses {
    /// Creates a unique ordered collection of embedded input responses.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` for an empty or duplicate key, or when more
    /// than [`HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND`] response values are
    /// given.
    pub fn new(entries: impl IntoIterator<Item = (String, MrtrInputResponse)>) -> McpResult<Self> {
        let mut result = Self::default();
        for (key, response) in entries {
            result.insert(key, response)?;
        }
        Ok(result)
    }

    /// Returns whether this collection contains no input responses.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Returns the number of input response values.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Looks up one response by its server-issued input key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&MrtrInputResponse> {
        self.index
            .get(key)
            .and_then(|index| self.entries.get(*index))
            .map(|(_, response)| response)
    }

    /// Iterates over input-response keys and values.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &MrtrInputResponse)> {
        self.entries
            .iter()
            .map(|(key, response)| (key.as_str(), response))
    }

    fn insert(&mut self, key: String, response: MrtrInputResponse) -> McpResult<()> {
        if key.is_empty()
            || self.entries.len() >= HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND
            || self.index.contains_key(&key)
        {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }
        self.index.insert(key.clone(), self.entries.len());
        self.entries.push((key, response));
        Ok(())
    }

    /// Appends one registry-admitted response only if this exchange has not
    /// accepted its key already. The registry has separately bounded total
    /// exchange growth, so this must not apply the single-round constructor
    /// ceiling to accumulated partial retries.
    fn append_accepted_if_absent(&mut self, key: &str, response: &MrtrInputResponse) -> bool {
        if self.index.contains_key(key) {
            return false;
        }
        self.index.insert(key.to_owned(), self.entries.len());
        self.entries.push((key.to_owned(), response.clone()));
        true
    }
}

impl Serialize for MrtrInputResponses {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.entries.len()))?;
        for (key, response) in &self.entries {
            map.serialize_entry(key, response)?;
        }
        map.end()
    }
}

/// A server-minted final MRTR request state.
///
/// The wire representation is opaque. Its `Debug` implementation is redacted,
/// and only [`MrtrInputRequired`] serializes it into a server response.
#[derive(Clone)]
pub struct MrtrRequestState(String);

impl std::fmt::Debug for MrtrRequestState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MrtrRequestState([redacted])")
    }
}

impl Serialize for MrtrRequestState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

/// A final `input_required` result emitted by the server.
///
/// Safe server construction always includes a protected request state. An
/// empty request map is emitted as a state-only result, which permits an
/// immediate retry without manufacturing an empty input map.
#[derive(Debug, Clone)]
pub struct MrtrInputRequired {
    input_requests: Option<MrtrInputRequests>,
    request_state: MrtrRequestState,
}

impl MrtrInputRequired {
    /// Returns the embedded request map, if this result needs client input.
    #[must_use]
    pub fn input_requests(&self) -> Option<&MrtrInputRequests> {
        self.input_requests.as_ref()
    }
}

impl Serialize for MrtrInputRequired {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut result = serializer.serialize_struct(
            "MrtrInputRequired",
            if self.input_requests.is_some() { 3 } else { 2 },
        )?;
        result.serialize_field("resultType", "input_required")?;
        if let Some(input_requests) = &self.input_requests {
            result.serialize_field("inputRequests", input_requests)?;
        }
        result.serialize_field("requestState", &self.request_state)?;
        result.end()
    }
}

/// The outcome of consuming one final MRTR retry.
#[derive(Debug, Clone)]
pub enum MrtrRetry {
    /// More client input is needed; only unsatisfied keys are reissued with a
    /// fresh request state.
    InputRequired(MrtrInputRequired),
    /// All currently requested input values were accepted and type-checked.
    Complete(MrtrCompletedInputs),
}

/// Accumulated, type-bound responses from a completed MRTR input exchange.
#[derive(Clone)]
pub struct MrtrCompletedInputs {
    responses: MrtrInputResponses,
    handler_request_state: Option<MrtrRequestState>,
}

impl std::fmt::Debug for MrtrCompletedInputs {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MrtrCompletedInputs")
            .field("responses", &self.responses)
            .field("handler_request_state", &self.handler_request_state)
            .finish()
    }
}

impl MrtrCompletedInputs {
    /// Private state returned by the handler that issued this exchange. It is
    /// never a downstream credential and becomes available only after the
    /// framework has admitted the bound, one-use continuation.
    #[cfg(any(feature = "proxy", test))]
    pub(crate) fn handler_request_state(&self) -> Option<&str> {
        self.handler_request_state
            .as_ref()
            .map(|state| state.0.as_str())
    }

    /// Returns every accepted response, including values accepted in earlier
    /// partial retries of this logical exchange.
    #[must_use]
    pub fn responses(&self) -> &MrtrInputResponses {
        &self.responses
    }

    /// Returns one framework-admitted elicitation response by its issued key.
    pub fn elicitation(&self, key: &str) -> McpResult<Option<fastmcp_protocol::ElicitResult>> {
        self.responses
            .get(key)
            .map(MrtrInputResponse::elicitation_result)
            .transpose()
    }

    /// Returns one framework-admitted sampling response by its issued key.
    pub fn sampling(
        &self,
        key: &str,
    ) -> McpResult<Option<fastmcp_protocol::FinalCreateMessageResult>> {
        self.responses
            .get(key)
            .map(MrtrInputResponse::sampling_result)
            .transpose()
    }

    /// Returns one framework-admitted roots response by its issued key.
    pub fn roots(&self, key: &str) -> McpResult<Option<fastmcp_protocol::ListRootsResult>> {
        self.responses
            .get(key)
            .map(MrtrInputResponse::roots_result)
            .transpose()
    }
}

#[derive(Debug, Clone)]
struct ExpectedInputLedger {
    kinds: BTreeMap<String, MrtrInputKind>,
}

impl ExpectedInputLedger {
    fn from_requests(requests: &MrtrInputRequests) -> Self {
        Self {
            kinds: requests
                .entries
                .iter()
                .map(|(key, request)| (key.clone(), request.kind()))
                .collect(),
        }
    }

    fn get(&self, key: &str) -> Option<MrtrInputKind> {
        self.kinds.get(key).copied()
    }

    fn is_empty(&self) -> bool {
        self.kinds.is_empty()
    }

    fn has_elicitation(&self) -> bool {
        self.kinds
            .values()
            .any(|kind| *kind == MrtrInputKind::Elicitation)
    }
}

#[derive(Debug, Clone)]
struct MrtrExchange {
    // Only cancellation invalidates a continuation. Normal response
    // finalization ends the old JSON-RPC request before the client can send
    // its new-ID retry, so treating it as cancellation would make every MRTR
    // exchange unusable.
    owner_cancellation: McpRequestCancellation,
    expires_at: Instant,
    round: u8,
    total_input_requests: usize,
    requests: MrtrInputRequests,
    expected: ExpectedInputLedger,
    responses: MrtrInputResponses,
    binding: Option<MrtrExchangeBinding>,
    handler_request_state: Option<MrtrRequestState>,
}

#[derive(Debug, Default)]
struct MrtrExchangeState {
    exchanges: HashMap<String, MrtrExchange>,
    stateless_closed: bool,
}

/// Process-local, one-use final MRTR request-state storage.
///
/// A record owns the exact key-to-response-kind ledger for the inputs it
/// emitted, including each final elicitation's mode and admitted form schema.
/// The opaque state is random, expires, cannot be replayed after a successful
/// retry, and is invalidated when its owning request is cancelled.
pub struct MrtrExchangeRegistry {
    state: Mutex<MrtrExchangeState>,
    max_states: usize,
    max_rounds: u8,
    max_inputs_per_round: usize,
    max_total_input_requests: usize,
    request_state_ttl: Duration,
}

impl MrtrExchangeRegistry {
    /// Creates a registry with the final protocol's default bounded policy.
    #[must_use]
    pub fn new() -> Self {
        Self {
            state: Mutex::new(MrtrExchangeState::default()),
            max_states: DEFAULT_MAX_MRTR_REQUEST_STATES,
            max_rounds: DEFAULT_MAX_MRTR_ROUNDS,
            max_inputs_per_round: DEFAULT_MAX_MRTR_INPUT_REQUESTS_PER_ROUND,
            max_total_input_requests: DEFAULT_MAX_MRTR_INPUT_REQUESTS_TOTAL,
            request_state_ttl: DEFAULT_MRTR_REQUEST_STATE_TTL,
        }
    }

    /// Creates a registry with caller-selected bounded limits.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` when a limit is zero or exceeds the final
    /// hard ceiling.
    pub fn with_limits(
        max_states: usize,
        max_rounds: u8,
        max_inputs_per_round: usize,
        max_total_input_requests: usize,
        request_state_ttl: Duration,
    ) -> McpResult<Self> {
        if !(1..=HARD_MAX_MRTR_REQUEST_STATES).contains(&max_states)
            || !(1..=HARD_MAX_MRTR_ROUNDS).contains(&max_rounds)
            || !(1..=HARD_MAX_MRTR_INPUT_REQUESTS_PER_ROUND).contains(&max_inputs_per_round)
            || !(max_inputs_per_round..=HARD_MAX_MRTR_INPUT_REQUESTS_TOTAL)
                .contains(&max_total_input_requests)
            || request_state_ttl.is_zero()
            || request_state_ttl > HARD_MAX_MRTR_REQUEST_STATE_TTL
        {
            return Err(McpError::invalid_params(INVALID_MRTR_LIMIT_ERROR));
        }

        Ok(Self {
            state: Mutex::new(MrtrExchangeState::default()),
            max_states,
            max_rounds,
            max_inputs_per_round,
            max_total_input_requests,
            request_state_ttl,
        })
    }

    /// Issues an `input_required` result bound to the owning request.
    ///
    /// This never sends an independent JSON-RPC request. The returned state
    /// retains the issued request map and exact key-to-response-kind ledger for
    /// a later retry.
    ///
    /// # Errors
    ///
    /// Returns `RequestCancelled` if the owner was already cancelled,
    /// `InvalidParams` when the map exceeds the configured round bound, or an
    /// internal error if secure state generation fails.
    pub fn issue(
        &self,
        owner_cancellation: McpRequestCancellation,
        input_requests: MrtrInputRequests,
    ) -> McpResult<MrtrInputRequired> {
        self.issue_at(
            owner_cancellation,
            None,
            input_requests,
            None,
            Instant::now(),
        )
    }

    /// Issues an `input_required` result bound to one router-admitted modern
    /// operation. Only [`Self::accept_wire_bound`] can consume such a state.
    pub(crate) fn issue_bound(
        &self,
        owner_cancellation: McpRequestCancellation,
        binding: MrtrExchangeBinding,
        input_requests: MrtrInputRequests,
        handler_request_state: Option<String>,
    ) -> McpResult<MrtrInputRequired> {
        self.issue_at(
            owner_cancellation,
            Some(binding),
            input_requests,
            handler_request_state,
            Instant::now(),
        )
    }

    /// Consumes one client retry's input-response map.
    ///
    /// Unknown keys are inert and ignored after bounded structural admission.
    /// Every recognized key must carry the exact response kind recorded when
    /// it was issued. Accepted final elicitation content must also satisfy its
    /// retained mode and form schema before any response in the map is stored.
    /// Partial maps are accepted and yield a fresh state with only unsatisfied
    /// descriptors.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` for unknown, expired, oversized, replayed, or
    /// wrong-kind state/input combinations, and `RequestCancelled` if the
    /// owning request cancellation wins before completion.
    pub fn accept(
        &self,
        request_state: &str,
        input_responses: MrtrInputResponses,
    ) -> McpResult<MrtrRetry> {
        self.accept_at(request_state, None, input_responses, false, Instant::now())
    }

    /// Decodes and consumes one router-admitted final `inputResponses` map.
    ///
    /// Final request parameter types retain input responses as JSON values.
    /// This boundary keeps router code from selecting a response kind itself:
    /// each recognized value is decoded using the kind retained for its
    /// server-issued input key. Unknown keys remain inert, and malformed or
    /// cross-kind values leave the request state available for a valid retry.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParams` if the state is invalid, the response map
    /// exceeds this registry's configured bound, or a recognized response
    /// cannot be decoded as the kind that was issued for its key.
    pub fn accept_wire(
        &self,
        request_state: &str,
        input_responses: &BTreeMap<String, serde_json::Value>,
    ) -> McpResult<MrtrRetry> {
        self.accept_wire_with_binding(request_state, None, input_responses)
    }

    /// Decodes and consumes a router retry only when its immutable request
    /// facts exactly match the state that issued it.
    pub(crate) fn accept_wire_bound(
        &self,
        request_state: &str,
        binding: &MrtrExchangeBinding,
        input_responses: &BTreeMap<String, serde_json::Value>,
    ) -> McpResult<MrtrRetry> {
        self.accept_wire_with_binding(request_state, Some(binding), input_responses)
    }

    /// Decodes and consumes ordered protocol response entries without first
    /// collapsing them into a map.
    ///
    /// The final protocol decoder has already rejected duplicate keys and
    /// retained their wire order. Keeping that representation through this
    /// admission boundary ensures a raw retry cannot be normalized before the
    /// continuation registry has validated it.
    pub(crate) fn accept_final_input_responses_bound(
        &self,
        request_state: &str,
        binding: &MrtrExchangeBinding,
        input_responses: &FinalInputResponses,
    ) -> McpResult<MrtrRetry> {
        let entries = input_responses
            .entries()
            .iter()
            .map(|(key, response)| {
                serde_json::to_value(response)
                    .map(|value| (key.clone(), value))
                    .map_err(|_| {
                        McpError::internal_error(
                            "final MRTR response could not be encoded for registry admission",
                        )
                    })
            })
            .collect::<McpResult<Vec<_>>>()?;
        self.accept_wire_entries_with_binding(
            request_state,
            Some(binding),
            entries.len(),
            entries.iter().map(|(key, value)| (key, value)),
        )
    }

    /// Consumes a retry whose `inputResponses` member was absent.
    ///
    /// The distinct entry point preserves the final wire contract: only an
    /// absent member can resume a state-only exchange. An explicitly present
    /// empty map still flows through [`Self::accept_wire_bound`] and remains
    /// invalid rather than silently becoming a state-only retry.
    pub(crate) fn accept_state_only_bound(
        &self,
        request_state: &str,
        binding: &MrtrExchangeBinding,
    ) -> McpResult<MrtrRetry> {
        self.accept_at(
            request_state,
            Some(binding),
            MrtrInputResponses::default(),
            true,
            Instant::now(),
        )
    }

    /// Returns the configured response-map admission ceiling so the router can
    /// reject an oversized raw map before typed request decoding allocates it.
    #[must_use]
    pub(crate) const fn max_inputs_per_round(&self) -> usize {
        self.max_inputs_per_round
    }

    fn accept_wire_with_binding(
        &self,
        request_state: &str,
        binding: Option<&MrtrExchangeBinding>,
        input_responses: &BTreeMap<String, serde_json::Value>,
    ) -> McpResult<MrtrRetry> {
        self.accept_wire_entries_with_binding(
            request_state,
            binding,
            input_responses.len(),
            input_responses.iter(),
        )
    }

    fn accept_wire_entries_with_binding<'a, I>(
        &self,
        request_state: &str,
        binding: Option<&MrtrExchangeBinding>,
        input_responses_len: usize,
        input_responses: I,
    ) -> McpResult<MrtrRetry>
    where
        I: IntoIterator<Item = (&'a String, &'a serde_json::Value)>,
    {
        if binding.is_some_and(|binding| binding.is_stateless && binding.principal_digest.is_none())
        {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if request_state.len() > DEFAULT_MAX_MRTR_REQUEST_STATE_BYTES {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if input_responses_len > self.max_inputs_per_round {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }

        let expected = {
            let mut state = self.lock_state();
            let now = Instant::now();
            let exchange = state
                .exchanges
                .get(request_state)
                .cloned()
                .ok_or_else(|| McpError::invalid_params(MRTR_REQUEST_STATE_ERROR))?;
            if exchange.binding.as_ref() != binding {
                return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
            }
            if now >= exchange.expires_at || exchange.owner_cancellation.is_cancel_requested() {
                state.exchanges.remove(request_state);
                return if exchange.owner_cancellation.is_cancel_requested() {
                    Err(McpError::request_cancelled())
                } else {
                    Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR))
                };
            }
            if binding.is_some_and(MrtrExchangeBinding::is_stateless)
                && exchange.expected.has_elicitation()
            {
                return Err(McpError::invalid_request(
                    "stateless HTTP cannot resume elicitation requestState; a durable MCP transport connection is required",
                ));
            }
            Self::purge_stale(&mut state, now);
            exchange.expected
        };

        let mut typed_responses = MrtrInputResponses::default();
        for (key, value) in input_responses {
            let Some(kind) = expected.get(key) else {
                continue;
            };
            typed_responses.insert(
                key.clone(),
                MrtrInputResponse::from_wire(kind, value.clone())?,
            )?;
        }

        // A map that names none of the outstanding inputs is not a partial
        // retry. Rotating it would burn a valid continuation without making
        // progress, so reject it before the current state can be consumed.
        if typed_responses.is_empty() {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }

        self.accept_at(
            request_state,
            binding,
            typed_responses,
            false,
            Instant::now(),
        )
    }

    /// Returns the number of non-expired, non-cancelled exchanges currently
    /// retained by this process-local registry.
    #[must_use]
    pub fn active_len(&self) -> usize {
        let mut state = self.lock_state();
        Self::purge_stale(&mut state, Instant::now());
        state.exchanges.len()
    }

    /// Permanently closes stateless issuance for this registry and removes
    /// every already-issued stateless exchange.
    ///
    /// The closed flag and purge share the registry lock with issuance. An
    /// in-flight handler therefore either publishes before this fence and is
    /// purged, or observes the fence and cannot mint a late continuation.
    pub(crate) fn close_stateless(&self) -> usize {
        let mut state = self.lock_state();
        state.stateless_closed = true;
        let active_before = state.exchanges.len();
        state.exchanges.retain(|_, exchange| {
            !exchange
                .binding
                .as_ref()
                .is_some_and(MrtrExchangeBinding::is_stateless)
        });
        active_before.saturating_sub(state.exchanges.len())
    }

    fn issue_at(
        &self,
        owner_cancellation: McpRequestCancellation,
        binding: Option<MrtrExchangeBinding>,
        input_requests: MrtrInputRequests,
        handler_request_state: Option<String>,
        now: Instant,
    ) -> McpResult<MrtrInputRequired> {
        // This registry retains private, one-use state even for stateless
        // HTTP. Require verified owner authority before cleanup or allocation.
        if binding
            .as_ref()
            .is_some_and(|binding| binding.is_stateless && binding.principal_digest.is_none())
        {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if owner_cancellation.is_cancel_requested() {
            return Err(McpError::request_cancelled());
        }
        if input_requests.len() > self.max_inputs_per_round {
            return Err(McpError::invalid_params(MRTR_ROUND_LIMIT_ERROR));
        }
        if handler_request_state
            .as_ref()
            .is_some_and(|value| value.len() > DEFAULT_MAX_MRTR_REQUEST_STATE_BYTES)
        {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }

        let expires_at = now
            .checked_add(self.request_state_ttl)
            .ok_or_else(|| McpError::internal_error(MRTR_REQUEST_STATE_UNAVAILABLE_ERROR))?;
        let mut state = self.lock_state();
        Self::purge_stale(&mut state, now);
        if state.stateless_closed
            && binding
                .as_ref()
                .is_some_and(MrtrExchangeBinding::is_stateless)
        {
            return Err(McpError::request_cancelled());
        }
        if state.exchanges.len() >= self.max_states {
            return Err(McpError::internal_error(MRTR_ROUND_LIMIT_ERROR));
        }

        let request_state = Self::allocate_request_state(&state)?;
        let expected = ExpectedInputLedger::from_requests(&input_requests);
        let input_requests = (!input_requests.is_empty()).then_some(input_requests);
        state.exchanges.insert(
            request_state.0.clone(),
            MrtrExchange {
                owner_cancellation,
                expires_at,
                round: 1,
                total_input_requests: input_requests.as_ref().map_or(0, MrtrInputRequests::len),
                expected,
                requests: input_requests.clone().unwrap_or_default(),
                responses: MrtrInputResponses::default(),
                binding,
                handler_request_state: handler_request_state.map(MrtrRequestState),
            },
        );
        Ok(MrtrInputRequired {
            input_requests,
            request_state,
        })
    }

    fn accept_at(
        &self,
        request_state: &str,
        binding: Option<&MrtrExchangeBinding>,
        input_responses: MrtrInputResponses,
        state_only_retry: bool,
        now: Instant,
    ) -> McpResult<MrtrRetry> {
        if binding.is_some_and(|binding| binding.is_stateless && binding.principal_digest.is_none())
        {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if request_state.len() > DEFAULT_MAX_MRTR_REQUEST_STATE_BYTES {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if input_responses.len() > self.max_inputs_per_round {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }

        let mut state = self.lock_state();
        let Some(exchange) = state.exchanges.get(request_state).cloned() else {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        };
        if exchange.binding.as_ref() != binding {
            return Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR));
        }
        if now >= exchange.expires_at || exchange.owner_cancellation.is_cancel_requested() {
            state.exchanges.remove(request_state);
            return if exchange.owner_cancellation.is_cancel_requested() {
                Err(McpError::request_cancelled())
            } else {
                Err(McpError::invalid_params(MRTR_REQUEST_STATE_ERROR))
            };
        }
        if binding.is_some_and(MrtrExchangeBinding::is_stateless)
            && exchange.expected.has_elicitation()
        {
            return Err(McpError::invalid_request(
                "stateless HTTP cannot resume elicitation requestState; a durable MCP transport connection is required",
            ));
        }
        if state_only_retry && (!exchange.expected.is_empty() || !exchange.requests.is_empty()) {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }

        let mut accepted_responses = exchange.responses.clone();
        let mut made_progress = false;
        for (key, response) in input_responses.iter() {
            if let Some(expected_kind) = exchange.expected.get(key) {
                if expected_kind != response.kind() {
                    return Err(McpError::invalid_params(MRTR_RESPONSE_KIND_ERROR));
                }
                let request = exchange
                    .requests
                    .get(key)
                    .ok_or_else(|| McpError::internal_error(MRTR_REQUEST_STATE_ERROR))?;
                request.validate_response(response)?;
                if accepted_responses.append_accepted_if_absent(key, response) {
                    made_progress = true;
                }
            }
        }

        if exchange.owner_cancellation.is_cancel_requested() {
            state.exchanges.remove(request_state);
            return Err(McpError::request_cancelled());
        }

        // Unknown-only (or otherwise no-progress) typed retries must be as
        // inert as their wire-decoded counterparts. Rotating here would burn
        // the caller's valid continuation without accepting any outstanding
        // input response.
        if !exchange.requests.is_empty() && !made_progress {
            return Err(McpError::invalid_params(MRTR_INPUT_MAP_ERROR));
        }

        let missing_requests = exchange.requests.unresolved_after(&accepted_responses);
        if missing_requests.is_empty() {
            state.exchanges.remove(request_state);
            return Ok(MrtrRetry::Complete(MrtrCompletedInputs {
                responses: accepted_responses,
                handler_request_state: exchange.handler_request_state,
            }));
        }

        let next_round = exchange
            .round
            .checked_add(1)
            .ok_or_else(|| McpError::invalid_params(MRTR_ROUND_LIMIT_ERROR))?;
        let next_total = exchange
            .total_input_requests
            .checked_add(missing_requests.len())
            .ok_or_else(|| McpError::invalid_params(MRTR_ROUND_LIMIT_ERROR))?;
        if next_round > self.max_rounds
            || missing_requests.len() > self.max_inputs_per_round
            || next_total > self.max_total_input_requests
        {
            state.exchanges.remove(request_state);
            return Err(McpError::invalid_params(MRTR_ROUND_LIMIT_ERROR));
        }

        // Generate the successor before consuming the current state. An RNG
        // failure therefore leaves the original exchange intact and does not
        // create a state/ledger gap.
        let next_state = Self::allocate_request_state(&state)?;
        let successor = MrtrExchange {
            owner_cancellation: exchange.owner_cancellation,
            expires_at: exchange.expires_at,
            round: next_round,
            total_input_requests: next_total,
            expected: ExpectedInputLedger::from_requests(&missing_requests),
            requests: missing_requests.clone(),
            responses: accepted_responses,
            binding: exchange.binding,
            handler_request_state: exchange.handler_request_state,
        };
        state.exchanges.remove(request_state);
        state.exchanges.insert(next_state.0.clone(), successor);

        Ok(MrtrRetry::InputRequired(MrtrInputRequired {
            input_requests: Some(missing_requests),
            request_state: next_state,
        }))
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, MrtrExchangeState> {
        match self.state.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    fn purge_stale(state: &mut MrtrExchangeState, now: Instant) {
        state.exchanges.retain(|_, exchange| {
            now < exchange.expires_at && !exchange.owner_cancellation.is_cancel_requested()
        });
    }

    fn allocate_request_state(state: &MrtrExchangeState) -> McpResult<MrtrRequestState> {
        for _ in 0..4 {
            let identifier = draw_security_identifier()
                .map_err(|_| McpError::internal_error(MRTR_REQUEST_STATE_UNAVAILABLE_ERROR))?;
            let encoded =
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(identifier.as_bytes());
            if !state.exchanges.contains_key(&encoded) {
                return Ok(MrtrRequestState(encoded));
            }
        }
        Err(McpError::internal_error(
            MRTR_REQUEST_STATE_UNAVAILABLE_ERROR,
        ))
    }
}

impl std::fmt::Debug for MrtrExchangeRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MrtrExchangeRegistry")
            .field("active_len", &self.active_len())
            .field("max_states", &self.max_states)
            .field("max_rounds", &self.max_rounds)
            .field("max_inputs_per_round", &self.max_inputs_per_round)
            .field("max_total_input_requests", &self.max_total_input_requests)
            .finish()
    }
}

impl Default for MrtrExchangeRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Dual-era Server-to-client Boundary
// ============================================================================

/// The typed outcome of one server-to-client input request.
///
/// Exact MCP 2024-11-05 completes the reverse JSON-RPC request and returns
/// its typed client response. MCP 2026-07-28 instead returns a server result
/// carrying an `input_required` exchange for the client to retry.
#[derive(Debug, Clone)]
pub enum DualEraServerToClientResult<T> {
    /// A response to an exact-2024 reverse JSON-RPC request.
    Legacy(T),
    /// A final-era result that requires the client to retry with input.
    InputRequired(MrtrInputRequired),
}

/// Era-selected server-to-client input boundary.
///
/// The exact MCP 2024-11-05 variant owns the transport sender and may issue
/// only `sampling/createMessage`, `elicitation/create`, and `roots/list`
/// reverse JSON-RPC requests. The MCP 2026-07-28 variant deliberately does
/// not retain that sender: it can only issue and consume the bounded
/// [`MrtrExchangeRegistry`] `input_required` retry flow.
#[derive(Clone)]
pub enum DualEraServerToClient {
    /// Exact MCP 2024-11-05 reverse JSON-RPC support.
    Legacy2024 {
        /// The connection's reverse-request sender.
        sender: RequestSender,
    },
    /// MCP 2026-07-28 embedded input/retry support.
    Modern2026 {
        /// The server-local registry for bounded input exchanges.
        exchanges: Arc<MrtrExchangeRegistry>,
    },
}

impl DualEraServerToClient {
    /// Selects the sole server-to-client mechanism for one negotiated era.
    ///
    /// The legacy sender is intentionally consumed and discarded for MCP
    /// 2026-07-28. That makes reverse JSON-RPC unavailable in the final-era
    /// variant even if the connection also has a transport send callback.
    #[must_use]
    pub fn new(
        era: ProtocolEra,
        legacy_sender: RequestSender,
        exchanges: Arc<MrtrExchangeRegistry>,
    ) -> Self {
        match era {
            ProtocolEra::Legacy2024 => Self::Legacy2024 {
                sender: legacy_sender,
            },
            ProtocolEra::Modern2026 => Self::Modern2026 { exchanges },
        }
    }

    /// Returns the exact negotiated era selected by this boundary.
    #[must_use]
    pub const fn era(&self) -> ProtocolEra {
        match self {
            Self::Legacy2024 { .. } => ProtocolEra::Legacy2024,
            Self::Modern2026 { .. } => ProtocolEra::Modern2026,
        }
    }

    /// Requests a client sampling completion.
    ///
    /// In the legacy era this sends `sampling/createMessage` directly. In the
    /// final era it returns an `input_required` result whose input descriptor
    /// has that exact method and is owned by `owner_cancellation`.
    pub async fn sampling_create_message(
        &self,
        cx: &Cx,
        owner_cancellation: McpRequestCancellation,
        input_key: impl Into<String>,
        params: DualEraSamplingParams,
    ) -> McpResult<DualEraServerToClientResult<fastmcp_protocol::CreateMessageResult>> {
        let input = match (self, params) {
            (Self::Legacy2024 { .. }, DualEraSamplingParams::Legacy2024(params)) => {
                MrtrInputRequest::with_params(MrtrInputKind::Sampling, params)?
            }
            (Self::Modern2026 { .. }, DualEraSamplingParams::Modern2026(params)) => {
                MrtrInputRequest::sampling(params)?
            }
            _ => {
                return Err(McpError::invalid_params(
                    "Sampling parameters do not match the negotiated protocol era",
                ));
            }
        };
        self.dispatch(cx, owner_cancellation, input_key.into(), input)
            .await
    }

    /// Requests client elicitation input.
    ///
    /// In the legacy era this sends `elicitation/create` directly. In the
    /// final era it returns an `input_required` result whose input descriptor
    /// has that exact method and is owned by `owner_cancellation`.
    pub async fn elicitation_create(
        &self,
        cx: &Cx,
        owner_cancellation: McpRequestCancellation,
        input_key: impl Into<String>,
        params: DualEraElicitationParams,
    ) -> McpResult<DualEraServerToClientResult<fastmcp_protocol::ElicitResult>> {
        let input = match (self, params) {
            (Self::Legacy2024 { .. }, DualEraElicitationParams::Legacy2024(params)) => {
                MrtrInputRequest::legacy_elicitation(params)
            }
            (Self::Modern2026 { .. }, DualEraElicitationParams::Modern2026(params)) => {
                MrtrInputRequest::final_elicitation(params)?
            }
            _ => return Err(McpError::invalid_params(INVALID_ELICITATION_REQUEST_ERROR)),
        };
        self.dispatch(cx, owner_cancellation, input_key.into(), input)
            .await
    }

    /// Requests the client's filesystem roots.
    ///
    /// In the legacy era this sends `roots/list` directly. In the final era
    /// it returns an `input_required` result whose input descriptor has that
    /// exact method and is owned by `owner_cancellation`.
    pub async fn roots_list(
        &self,
        cx: &Cx,
        owner_cancellation: McpRequestCancellation,
        input_key: impl Into<String>,
    ) -> McpResult<DualEraServerToClientResult<fastmcp_protocol::ListRootsResult>> {
        self.dispatch(
            cx,
            owner_cancellation,
            input_key.into(),
            MrtrInputRequest::roots(),
        )
        .await
    }

    /// Consumes one final-era `inputResponses` retry.
    ///
    /// This accepts retries only for MCP 2026-07-28. The caller supplies the
    /// active [`Cx`] for cancellation/budget authority; the registry retains
    /// the request-local cancellation owner that was bound when it issued the
    /// corresponding `input_required` result.
    pub fn accept_input_retry(
        &self,
        cx: &Cx,
        request_state: &str,
        input_responses: MrtrInputResponses,
    ) -> McpResult<MrtrRetry> {
        if cx.checkpoint().is_err() {
            return Err(McpError::request_cancelled());
        }

        match self {
            Self::Legacy2024 { .. } => Err(McpError::invalid_params(LEGACY_INPUT_RETRY_ERROR)),
            Self::Modern2026 { exchanges } => exchanges.accept(request_state, input_responses),
        }
    }

    async fn dispatch<T: DeserializeOwned>(
        &self,
        cx: &Cx,
        owner_cancellation: McpRequestCancellation,
        input_key: String,
        input_request: MrtrInputRequest,
    ) -> McpResult<DualEraServerToClientResult<T>> {
        if cx.checkpoint().is_err() || owner_cancellation.is_cancel_requested() {
            return Err(McpError::request_cancelled());
        }

        match self {
            Self::Legacy2024 { sender } => {
                let method = input_request.kind().method();
                let params = input_request.into_legacy_params()?;
                let response = sender
                    .for_request(owner_cancellation)
                    .send_request(cx, method, params)
                    .await?;
                Ok(DualEraServerToClientResult::Legacy(response))
            }
            Self::Modern2026 { exchanges } => {
                let input_requests = MrtrInputRequests::new([(input_key, input_request)])?;
                exchanges
                    .issue(owner_cancellation, input_requests)
                    .map(DualEraServerToClientResult::InputRequired)
            }
        }
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use fastmcp_core::block_on;

    fn receive_pending(mut receiver: ResponseReceiver) -> PendingResponse {
        let cx = Cx::for_testing();
        block_on(receiver.recv(&cx)).expect("pending response channel must remain connected")
    }

    fn mrtr_state_from_wire(result: &MrtrInputRequired) -> String {
        serde_json::to_value(result)
            .expect("MRTR result must serialize")
            .get("requestState")
            .and_then(serde_json::Value::as_str)
            .expect("MRTR result must contain opaque request state")
            .to_owned()
    }

    fn mrtr_roots_response() -> MrtrInputResponse {
        MrtrInputResponse::roots(fastmcp_protocol::ListRootsResult::empty())
            .expect("roots response must serialize")
    }

    fn final_form_elicitation_params() -> fastmcp_protocol::FinalEmbeddedElicitationParams {
        serde_json::from_value(serde_json::json!({
            "mode": "form",
            "message": "Choose a display name",
            "requestedSchema": {
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "properties": {"displayName": {"type": "string", "minLength": 1}},
                "required": ["displayName"],
            },
        }))
        .expect("final form elicitation parameters must admit")
    }

    fn final_url_elicitation_params() -> fastmcp_protocol::FinalEmbeddedElicitationParams {
        serde_json::from_value(serde_json::json!({
            "mode": "url",
            "message": "Authorize access",
            "url": "https://example.com/authorize",
        }))
        .expect("final URL elicitation parameters must admit")
    }

    fn final_sampling_params() -> fastmcp_protocol::FinalEmbeddedCreateMessageParams {
        serde_json::from_value(serde_json::json!({
            "messages": [{
                "role": "assistant",
                "content": {
                    "type": "tool_use",
                    "id": "weather-1",
                    "name": "weather",
                    "input": {"city": "Boston"},
                },
            }],
            "maxTokens": 16,
            "tools": [{
                "name": "weather",
                "inputSchema": {"type": "object"},
            }],
            "toolChoice": {"mode": "required"},
        }))
        .expect("final sampling parameters with tool use must admit")
    }

    fn final_sampling_result() -> fastmcp_protocol::FinalCreateMessageResult {
        serde_json::from_value(serde_json::json!({
            "content": {
                "type": "tool_use",
                "id": "weather-2",
                "name": "weather",
                "input": {"city": "Cambridge"},
            },
            "role": "assistant",
            "model": "test-model",
        }))
        .expect("final sampling result with tool use must admit")
    }

    #[test]
    fn mrtr_form_content_rejection_preserves_the_entire_pending_batch() {
        let registry = MrtrExchangeRegistry::new();
        let required = registry
            .issue(
                McpRequestCancellation::new(),
                MrtrInputRequests::new([
                    (
                        "form".to_owned(),
                        MrtrInputRequest::final_elicitation(final_form_elicitation_params())
                            .expect("form request admits"),
                    ),
                    ("roots".to_owned(), MrtrInputRequest::roots()),
                ])
                .expect("two input requests admit"),
            )
            .expect("form and roots exchange issues");
        let state = mrtr_state_from_wire(&required);
        for invalid in [
            serde_json::json!({"action": "accept"}),
            serde_json::json!({"action": "accept", "content": {}}),
            serde_json::json!({"action": "accept", "content": {"displayName": true}}),
            serde_json::json!({"action": "accept", "content": {"displayName": ""}}),
            serde_json::json!({"action": "accept", "content": null}),
            serde_json::json!({"action": "accept", "content": "private-invalid-answer"}),
        ] {
            let invalid = MrtrInputResponses::new([
                // This earlier sibling must not be committed when the form
                // later in the same typed batch fails admission.
                ("roots".to_owned(), mrtr_roots_response()),
                (
                    "form".to_owned(),
                    MrtrInputResponse {
                        kind: MrtrInputKind::Elicitation,
                        value: invalid,
                    },
                ),
            ])
            .expect("typed map preserves response order");
            let error = registry
                .accept(&state, invalid)
                .expect_err("unaccepted form content cannot consume an exchange");
            assert_eq!(error.code, McpErrorCode::InvalidParams);
            assert_eq!(error.message, MRTR_RESPONSE_KIND_ERROR);
            assert_eq!(registry.active_len(), 1);
        }

        let accepted = MrtrInputResponse::elicitation(fastmcp_protocol::ElicitResult::accept(
            HashMap::from([(
                "displayName".to_owned(),
                fastmcp_protocol::ElicitContentValue::String("Ada".to_owned()),
            )]),
        ))
        .expect("valid form response encodes");
        let partial = registry
            .accept(
                &state,
                MrtrInputResponses::new([("form".to_owned(), accepted)])
                    .expect("one valid form response"),
            )
            .expect("original state accepts a corrected form");
        let MrtrRetry::InputRequired(partial) = partial else {
            panic!("the roots sibling from rejected batches must still be pending");
        };
        assert_eq!(partial.input_requests().expect("roots remain").len(), 1);
        assert!(
            partial
                .input_requests()
                .expect("roots remain")
                .get("roots")
                .is_some()
        );
        let successor = mrtr_state_from_wire(&partial);
        let complete = registry
            .accept_wire(
                &successor,
                &BTreeMap::from([
                    // The previously accepted key is now inert. Its original
                    // accepted value must survive a later changed duplicate.
                    (
                        "form".to_owned(),
                        serde_json::json!({"action": "accept", "content": {}}),
                    ),
                    ("roots".to_owned(), serde_json::json!({"roots": []})),
                ]),
            )
            .expect("the outstanding roots response completes the successor");
        let MrtrRetry::Complete(complete) = complete else {
            panic!("all requested inputs are now answered");
        };
        let form = complete
            .elicitation("form")
            .expect("typed form response")
            .expect("form exists");
        assert_eq!(
            form.content.expect("accepted form content")["displayName"],
            fastmcp_protocol::ElicitContentValue::String("Ada".to_owned())
        );
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn mrtr_url_acceptance_rejects_content_without_consuming_retry_state() {
        let registry = MrtrExchangeRegistry::new();
        let required = registry
            .issue(
                McpRequestCancellation::new(),
                MrtrInputRequests::new([(
                    "url".to_owned(),
                    MrtrInputRequest::final_elicitation(final_url_elicitation_params())
                        .expect("URL request admits"),
                )])
                .expect("one URL input"),
            )
            .expect("URL exchange issues");
        let state = mrtr_state_from_wire(&required);
        for content in [
            serde_json::json!({}),
            serde_json::json!({"private": "do-not-echo"}),
            serde_json::Value::Null,
        ] {
            let error = registry
                .accept_wire(
                    &state,
                    &BTreeMap::from([(
                        "url".to_owned(),
                        serde_json::json!({"action": "accept", "content": content}),
                    )]),
                )
                .expect_err("accepted URL responses cannot carry form content");
            assert_eq!(error.code, McpErrorCode::InvalidParams);
            assert_eq!(error.message, MRTR_RESPONSE_KIND_ERROR);
            assert_eq!(registry.active_len(), 1);
        }
        let completed = registry
            .accept_wire(
                &state,
                &BTreeMap::from([(
                    "url".to_owned(),
                    serde_json::json!({"action": "accept"}),
                )]),
            )
            .expect("content-free URL acceptance uses the original state");
        let MrtrRetry::Complete(completed) = completed else {
            panic!("URL acceptance completes the single-input exchange");
        };
        let response = completed
            .elicitation("url")
            .expect("URL response decodes")
            .expect("URL response exists");
        assert_eq!(response.action, fastmcp_protocol::ElicitAction::Accept);
        assert!(response.content.is_none());
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn mrtr_decline_and_cancel_do_not_apply_accepted_form_schema_rules() {
        for params in [
            final_form_elicitation_params(),
            final_url_elicitation_params(),
        ] {
            for action in ["decline", "cancel"] {
                let registry = MrtrExchangeRegistry::new();
                let required = registry
                    .issue(
                        McpRequestCancellation::new(),
                        MrtrInputRequests::new([(
                            "input".to_owned(),
                            MrtrInputRequest::final_elicitation(params.clone())
                                .expect("request admits"),
                        )])
                        .expect("one elicitation input"),
                    )
                    .expect("elicitation exchange issues");
                let state = mrtr_state_from_wire(&required);
                for invalid_content in [serde_json::Value::Null, serde_json::json!([])] {
                    assert!(
                        registry
                            .accept_wire(
                                &state,
                                &BTreeMap::from([(
                                    "input".to_owned(),
                                    serde_json::json!({"action": action, "content": invalid_content}),
                                )]),
                            )
                            .is_err(),
                        "present content must still be an object"
                    );
                    assert_eq!(registry.active_len(), 1);
                }
                let complete = registry
                    .accept_wire(
                        &state,
                        &BTreeMap::from([(
                            "input".to_owned(),
                            // Structurally valid content, deliberately invalid
                            // for the form contract, remains an accepted peer
                            // SHOULD deviation for a non-accept action.
                            serde_json::json!({"action": action, "content": {"displayName": false}}),
                        )]),
                    )
                    .expect("decline/cancel need no accepted form content");
                let MrtrRetry::Complete(complete) = complete else {
                    panic!("decline/cancel answers the single input");
                };
                let response = complete
                    .elicitation("input")
                    .expect("response decodes")
                    .expect("response exists");
                assert_eq!(
                    response.action,
                    if action == "decline" {
                        fastmcp_protocol::ElicitAction::Decline
                    } else {
                        fastmcp_protocol::ElicitAction::Cancel
                    }
                );
                assert_eq!(registry.active_len(), 0);
            }
        }
    }

    #[test]
    fn mrtr_embeds_exact_input_maps_and_completes_with_bound_responses() {
        let registry = MrtrExchangeRegistry::new();
        let owner = McpRequestCancellation::new();
        let input_requests = MrtrInputRequests::new([
            (
                "elicit".to_owned(),
                MrtrInputRequest::final_elicitation(final_form_elicitation_params())
                    .expect("elicitation request must serialize"),
            ),
            (
                "sample".to_owned(),
                MrtrInputRequest::sampling(final_sampling_params())
                    .expect("sampling request must serialize"),
            ),
            ("roots".to_owned(), MrtrInputRequest::roots()),
        ])
        .expect("unique MRTR input map");

        let required = registry
            .issue(owner.clone(), input_requests)
            .expect("MRTR input result must issue");
        assert!(
            owner.begin_finalization(),
            "normal original-response finalization must not cancel MRTR state"
        );
        let wire = serde_json::to_value(&required).expect("MRTR result must serialize");
        assert_eq!(wire["resultType"], "input_required");
        assert_eq!(
            wire["inputRequests"]["elicit"]["method"],
            "elicitation/create"
        );
        assert_eq!(
            wire["inputRequests"]["sample"]["method"],
            "sampling/createMessage"
        );
        assert_eq!(
            wire["inputRequests"]["roots"],
            serde_json::json!({"method": "roots/list"})
        );
        for key in ["elicit", "sample", "roots"] {
            assert!(wire["inputRequests"][key].get("jsonrpc").is_none());
            assert!(wire["inputRequests"][key].get("id").is_none());
        }
        assert!(
            wire["inputRequests"]["sample"]["params"]
                .get("_meta")
                .is_none()
        );
        assert_eq!(
            wire["inputRequests"]["sample"]["params"]["toolChoice"],
            serde_json::json!({"mode": "required"}),
            "final sampling tool-choice controls must survive MRTR issuance"
        );
        assert_eq!(
            wire["inputRequests"]["sample"]["params"]["tools"][0]["name"], "weather",
            "final sampling tool declarations must survive MRTR issuance"
        );

        let request_state = mrtr_state_from_wire(&required);
        let partial_responses = MrtrInputResponses::new([
            (
                "elicit".to_owned(),
                MrtrInputResponse::elicitation(fastmcp_protocol::ElicitResult::decline())
                    .expect("elicitation response must serialize"),
            ),
            ("inert-unknown-key".to_owned(), mrtr_roots_response()),
        ])
        .expect("unique MRTR response map");
        let response_wire =
            serde_json::to_value(&partial_responses).expect("MRTR responses must serialize");
        assert_eq!(
            response_wire["elicit"],
            serde_json::json!({"action": "decline"})
        );

        let retry = registry
            .accept(&request_state, partial_responses)
            .expect("partial MRTR response map must reissue only missing inputs");
        let MrtrRetry::InputRequired(retry) = retry else {
            panic!("partial MRTR response map must not complete the exchange");
        };
        let retry_wire = serde_json::to_value(&retry).expect("retry result must serialize");
        assert!(retry_wire["inputRequests"].get("elicit").is_none());
        assert_eq!(
            retry_wire["inputRequests"]["sample"]["method"],
            "sampling/createMessage"
        );
        assert_eq!(
            retry_wire["inputRequests"]["roots"],
            serde_json::json!({"method": "roots/list"})
        );
        let retry_state = mrtr_state_from_wire(&retry);
        assert_ne!(
            retry_state, request_state,
            "partial retry needs fresh state"
        );
        let old_state_error = registry
            .accept(&request_state, MrtrInputResponses::default())
            .expect_err("the predecessor state must not replay after partial acceptance");
        assert_eq!(old_state_error.code, McpErrorCode::InvalidParams);

        let complete = registry
            .accept(
                &retry_state,
                MrtrInputResponses::new([
                    (
                        "sample".to_owned(),
                        MrtrInputResponse::sampling(final_sampling_result())
                            .expect("sampling response must serialize"),
                    ),
                    ("roots".to_owned(), mrtr_roots_response()),
                ])
                .expect("unique MRTR response map"),
            )
            .expect("matching remaining MRTR responses must complete");
        let MrtrRetry::Complete(complete) = complete else {
            panic!("all matching MRTR responses must complete the exchange");
        };
        assert_eq!(complete.responses().len(), 3);
        assert_eq!(
            complete
                .responses()
                .get("elicit")
                .map(MrtrInputResponse::kind),
            Some(MrtrInputKind::Elicitation)
        );
        assert_eq!(
            complete
                .responses()
                .get("sample")
                .map(MrtrInputResponse::kind),
            Some(MrtrInputKind::Sampling)
        );
        assert!(matches!(
            complete.sampling("sample"),
            Ok(Some(fastmcp_protocol::FinalCreateMessageResult {
                content: fastmcp_protocol::FinalSamplingMessageContent::Block(
                    fastmcp_protocol::FinalSamplingMessageContentBlock::ToolUse { .. }
                ),
                ..
            }))
        ));
        assert_eq!(
            complete
                .responses()
                .get("roots")
                .map(MrtrInputResponse::kind),
            Some(MrtrInputKind::Roots)
        );
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn mrtr_rejects_cross_kind_before_consumption_and_rejects_replay() {
        let registry = MrtrExchangeRegistry::new();
        let input_requests =
            MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                .expect("unique MRTR input map");
        let required = registry
            .issue(McpRequestCancellation::new(), input_requests)
            .expect("MRTR input result must issue");
        let request_state = mrtr_state_from_wire(&required);

        let wrong_kind = MrtrInputResponses::new([(
            "roots".to_owned(),
            MrtrInputResponse::sampling(final_sampling_result())
                .expect("sampling response must serialize"),
        )])
        .expect("unique MRTR response map");
        let error = registry
            .accept(&request_state, wrong_kind)
            .expect_err("a sampling value cannot fulfill a roots request");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            registry.active_len(),
            1,
            "wrong-kind input must not consume state"
        );

        let matching = MrtrInputResponses::new([("roots".to_owned(), mrtr_roots_response())])
            .expect("unique MRTR response map");
        assert!(matches!(
            registry.accept(&request_state, matching),
            Ok(MrtrRetry::Complete(_))
        ));
        assert_eq!(registry.active_len(), 0);

        let replay = registry
            .accept(
                &request_state,
                MrtrInputResponses::new([("roots".to_owned(), mrtr_roots_response())])
                    .expect("unique MRTR response map"),
            )
            .expect_err("a consumed MRTR request state must not replay");
        assert_eq!(replay.code, McpErrorCode::InvalidParams);
        assert_eq!(registry.active_len(), 0, "replay must not restore state");
    }

    #[test]
    fn mrtr_typed_unknown_only_retry_preserves_the_original_continuation() {
        let registry = MrtrExchangeRegistry::new();
        let required = registry
            .issue(
                McpRequestCancellation::new(),
                MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                    .expect("unique MRTR input map"),
            )
            .expect("MRTR input result must issue");
        let request_state = mrtr_state_from_wire(&required);

        let unknown_only = MrtrInputResponses::new([("inert".to_owned(), mrtr_roots_response())])
            .expect("typed response map permits an inert key");
        let error = registry
            .accept(&request_state, unknown_only)
            .expect_err("unknown-only typed input must not rotate a continuation");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            registry.active_len(),
            1,
            "the rejected unknown-only retry must retain the original state"
        );

        let matching = MrtrInputResponses::new([("roots".to_owned(), mrtr_roots_response())])
            .expect("unique matching response map");
        assert!(matches!(
            registry.accept(&request_state, matching),
            Ok(MrtrRetry::Complete(_))
        ));
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn mrtr_accept_wire_decodes_the_issued_kind_before_consuming_state() {
        let registry = MrtrExchangeRegistry::new();
        let required = registry
            .issue(
                McpRequestCancellation::new(),
                MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                    .expect("unique MRTR input map"),
            )
            .expect("MRTR input result must issue");
        let request_state = mrtr_state_from_wire(&required);

        let wrong_kind = BTreeMap::from([(
            "roots".to_owned(),
            serde_json::to_value(
                MrtrInputResponse::sampling(final_sampling_result())
                    .expect("sampling response must serialize"),
            )
            .expect("sampling response must convert to a wire value"),
        )]);
        let error = registry
            .accept_wire(&request_state, &wrong_kind)
            .expect_err("a sampling wire value cannot fulfill a roots request");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            registry.active_len(),
            1,
            "wrong-kind wire input must not consume state"
        );

        let matching = BTreeMap::from([(
            "roots".to_owned(),
            serde_json::to_value(mrtr_roots_response())
                .expect("roots response must convert to a wire value"),
        )]);
        assert!(matches!(
            registry.accept_wire(&request_state, &matching),
            Ok(MrtrRetry::Complete(_))
        ));
        assert_eq!(registry.active_len(), 0);

        let replay = registry
            .accept_wire(&request_state, &matching)
            .expect_err("a consumed wire request state must not replay");
        assert_eq!(replay.code, McpErrorCode::InvalidParams);
    }

    #[test]
    fn mrtr_accepts_ordered_final_input_responses_without_map_normalization() {
        let registry = MrtrExchangeRegistry::new();
        let binding = MrtrExchangeBinding::new(
            "tools/call",
            "ordered-tool".to_owned(),
            [3; 32],
            [4; 32],
            None,
            [5; 32],
        );
        let required = registry
            .issue_bound(
                McpRequestCancellation::new(),
                binding.clone(),
                MrtrInputRequests::new([
                    ("second".to_owned(), MrtrInputRequest::roots()),
                    ("first".to_owned(), MrtrInputRequest::roots()),
                ])
                .expect("unique MRTR input keys"),
                None,
            )
            .expect("MRTR input result must issue");
        let request_state = mrtr_state_from_wire(&required);
        let responses: FinalInputResponses =
            serde_json::from_str(r#"{"second":{"roots":[]},"first":{"roots":[]}}"#)
                .expect("ordered final responses decode");
        assert_eq!(
            responses
                .entries()
                .iter()
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            vec!["second", "first"],
            "the registry receives the protocol decoder's order-preserving representation"
        );
        let completed = registry
            .accept_final_input_responses_bound(&request_state, &binding, &responses)
            .expect("ordered protocol responses complete the exchange");
        let MrtrRetry::Complete(completed) = completed else {
            panic!("every expected ordered response completes the exchange");
        };
        assert_eq!(
            completed
                .responses()
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            vec!["second", "first"],
            "handler delivery preserves the accepted wire order rather than BTreeMap key order"
        );
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn closing_stateless_mrtr_is_latched_and_preserves_durable_exchanges() {
        let registry = MrtrExchangeRegistry::new();
        let input_requests = || {
            MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                .expect("unique MRTR input map")
        };
        let stateless_binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "stateless-tool".to_owned(),
            [1; 32],
            [2; 32],
            Some([3; 32]),
            [5; 32],
        );
        let stateless = registry
            .issue_bound(
                McpRequestCancellation::new(),
                stateless_binding.clone(),
                input_requests(),
                None,
            )
            .expect("stateless MRTR state issues before listener shutdown");
        let stateless_state = mrtr_state_from_wire(&stateless);

        let durable_binding = MrtrExchangeBinding::new(
            "tools/call",
            "durable-tool".to_owned(),
            [3; 32],
            [4; 32],
            None,
            [5; 32],
        );
        let durable = registry
            .issue_bound(
                McpRequestCancellation::new(),
                durable_binding.clone(),
                input_requests(),
                None,
            )
            .expect("durable MRTR state issues beside stateless state");
        let durable_state = mrtr_state_from_wire(&durable);
        assert_eq!(registry.active_len(), 2);

        assert_eq!(
            registry.close_stateless(),
            1,
            "listener shutdown removes exactly the retained stateless exchange"
        );
        assert_eq!(registry.active_len(), 1);
        let removed = registry
            .accept_wire_bound(
                &stateless_state,
                &stateless_binding,
                &BTreeMap::from([(
                    "roots".to_owned(),
                    serde_json::to_value(mrtr_roots_response())
                        .expect("roots response must serialize"),
                )]),
            )
            .expect_err("a purged stateless state cannot resume after shutdown");
        assert_eq!(removed.code, McpErrorCode::InvalidParams);

        let late_issue = registry
            .issue_bound(
                McpRequestCancellation::new(),
                stateless_binding,
                input_requests(),
                None,
            )
            .expect_err("the latched shutdown fence rejects late stateless issuance");
        assert_eq!(late_issue.code, McpErrorCode::RequestCancelled);
        assert_eq!(registry.close_stateless(), 0, "the fence is idempotent");

        assert!(matches!(
            registry.accept_wire_bound(
                &durable_state,
                &durable_binding,
                &BTreeMap::from([(
                    "roots".to_owned(),
                    serde_json::to_value(mrtr_roots_response())
                        .expect("roots response must serialize"),
                )]),
            ),
            Ok(MrtrRetry::Complete(_))
        ));
        assert_eq!(registry.active_len(), 0);
    }

    #[test]
    fn auth_00_ownerless_http_retry_rejects_before_expired_state_lookup() {
        let owner_binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "owner-bound-tool".to_owned(),
            [11; 32],
            [13; 32],
            Some([17; 32]),
            [19; 32],
        );
        let ownerless_binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "owner-bound-tool".to_owned(),
            [11; 32],
            [13; 32],
            None,
            [19; 32],
        );
        let response = BTreeMap::from([(
            "roots".to_owned(),
            serde_json::to_value(mrtr_roots_response()).expect("roots response must encode"),
        )]);

        for state_only in [false, true] {
            let registry = MrtrExchangeRegistry::new();
            let required = registry
                .issue_bound(
                    McpRequestCancellation::new(),
                    owner_binding.clone(),
                    MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                        .expect("unique input map"),
                    None,
                )
                .expect("trusted owner can issue private HTTP state");
            let request_state = mrtr_state_from_wire(&required);
            let expires_at = Instant::now() - Duration::from_secs(1);
            registry
                .lock_state()
                .exchanges
                .get_mut(&request_state)
                .expect("issued exchange")
                .expires_at = expires_at;
            let accept = |binding: &MrtrExchangeBinding| {
                if state_only {
                    registry.accept_state_only_bound(&request_state, binding)
                } else {
                    registry.accept_wire_bound(&request_state, binding, &response)
                }
            };

            let denied = accept(&ownerless_binding)
                .expect_err("ownerless admission must precede either lookup entry point");
            assert_eq!(denied.code, McpErrorCode::InvalidParams);
            {
                // active_len() purges expired entries; inspect authoritative
                // storage directly so the observation cannot mutate the oracle.
                let state = registry.lock_state();
                assert_eq!(state.exchanges.len(), 1);
                let retained = state.exchanges.get(&request_state).unwrap();
                assert_eq!(retained.expires_at, expires_at);
                assert_eq!(retained.binding.as_ref(), Some(&owner_binding));
                assert!(retained.responses.is_empty());
            }
            // Changing only the owner authority reaches the ordinary expiry
            // lookup and removes the same expired state in both entry points.
            let expired = accept(&owner_binding).expect_err("expired state must reject");
            assert_eq!(expired.code, McpErrorCode::InvalidParams);
            assert!(registry.lock_state().exchanges.is_empty());
        }
    }

    #[test]
    fn stateless_mrtr_method_binding_rejects_without_consuming_state() {
        let registry = MrtrExchangeRegistry::new();
        let binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "bound-target".to_owned(),
            [23; 32],
            [29; 32],
            Some([31; 32]),
            [37; 32],
        );
        let required = registry
            .issue_bound(
                McpRequestCancellation::new(),
                binding.clone(),
                MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                    .expect("unique bound input map"),
                None,
            )
            .expect("stateless method-bound state issues");
        let request_state = mrtr_state_from_wire(&required);
        let method_mismatch = MrtrExchangeBinding::stateless(
            "resources/read",
            "bound-target".to_owned(),
            [23; 32],
            [29; 32],
            Some([31; 32]),
            [37; 32],
        );
        let response = BTreeMap::from([(
            "roots".to_owned(),
            serde_json::to_value(mrtr_roots_response()).expect("roots response must serialize"),
        )]);

        let rejected = registry
            .accept_wire_bound(&request_state, &method_mismatch, &response)
            .expect_err("changing only the bound method must reject");
        assert_eq!(rejected.code, McpErrorCode::InvalidParams);
        assert_eq!(
            registry.active_len(),
            1,
            "a method-binding rejection must leave the state available"
        );
        assert!(matches!(
            registry.accept_wire_bound(&request_state, &binding, &response),
            Ok(MrtrRetry::Complete(_))
        ));
        assert_eq!(registry.active_len(), 0);
    }

    fn auth_00_expired_mrtr_owner_probe(foreign_owner: bool) {
        let owner_binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "expired-owner-tool".to_owned(),
            [41; 32],
            [43; 32],
            Some([47; 32]),
            [59; 32],
        );
        let mut caller_binding = owner_binding.clone();
        if foreign_owner {
            caller_binding.principal_digest = Some([53; 32]);
        }
        for state_only in [false, true] {
            let registry = MrtrExchangeRegistry::new();
            let cancellation = McpRequestCancellation::new();
            let initial = registry
                .issue_bound(
                    cancellation.clone(),
                    owner_binding.clone(),
                    MrtrInputRequests::new([
                        ("first".to_owned(), MrtrInputRequest::roots()),
                        ("second".to_owned(), MrtrInputRequest::roots()),
                    ])
                    .unwrap(),
                    Some("issuer-private-resume-state".to_owned()),
                )
                .expect("owner can issue a two-input exchange");
            let first_response = BTreeMap::from([(
                "first".to_owned(),
                serde_json::to_value(mrtr_roots_response()).unwrap(),
            )]);
            let partial = registry
                .accept_wire_bound(
                    &mrtr_state_from_wire(&initial),
                    &owner_binding,
                    &first_response,
                )
                .expect("first accepted input issues a real partial successor");
            let MrtrRetry::InputRequired(partial) = partial else {
                panic!("one missing input must retain a successor");
            };
            let request_state = mrtr_state_from_wire(&partial);
            let before = {
                let mut state = registry.lock_state();
                assert_eq!(state.exchanges.len(), 1);
                assert!(!state.stateless_closed);
                let exchange = state.exchanges.get_mut(&request_state).unwrap();
                exchange.expires_at = Instant::now() - Duration::from_secs(1);
                assert_eq!(exchange.round, 2);
                assert_eq!(exchange.responses.len(), 1);
                exchange.clone()
            };
            let response = BTreeMap::from([(
                "second".to_owned(),
                serde_json::to_value(mrtr_roots_response()).unwrap(),
            )]);
            let accept = |binding: &MrtrExchangeBinding| {
                if state_only {
                    registry.accept_state_only_bound(&request_state, binding)
                } else {
                    registry.accept_wire_bound(&request_state, binding, &response)
                }
            };
            let rejected = accept(&caller_binding).expect_err("expired or foreign state rejects");
            assert_eq!(rejected.code, McpErrorCode::InvalidParams);
            assert_eq!(rejected.message, MRTR_REQUEST_STATE_ERROR);
            let retained_owner = if foreign_owner {
                let state = registry.lock_state();
                assert_eq!(state.exchanges.len(), 1);
                assert!(!state.stateless_closed);
                let retained = state.exchanges.get(&request_state).unwrap();
                assert_eq!(retained.expires_at, before.expires_at);
                assert_eq!(retained.round, before.round);
                assert_eq!(retained.total_input_requests, before.total_input_requests);
                assert_eq!(retained.binding, before.binding);
                assert_eq!(retained.expected.kinds, before.expected.kinds);
                assert_eq!(
                    serde_json::to_vec(&retained.requests).unwrap(),
                    serde_json::to_vec(&before.requests).unwrap()
                );
                assert_eq!(
                    serde_json::to_vec(&retained.responses).unwrap(),
                    serde_json::to_vec(&before.responses).unwrap()
                );
                assert_eq!(
                    retained
                        .responses
                        .entries
                        .iter()
                        .map(|(key, response)| (key, response.kind))
                        .collect::<Vec<_>>(),
                    before
                        .responses
                        .entries
                        .iter()
                        .map(|(key, response)| (key, response.kind))
                        .collect::<Vec<_>>()
                );
                assert_eq!(retained.responses.index, before.responses.index);
                assert_eq!(
                    retained
                        .handler_request_state
                        .as_ref()
                        .map(|state| &state.0),
                    before.handler_request_state.as_ref().map(|state| &state.0)
                );
                assert_eq!(
                    retained.owner_cancellation.is_cancel_requested(),
                    before.owner_cancellation.is_cancel_requested()
                );
                assert_eq!(
                    retained.owner_cancellation.is_terminal(),
                    before.owner_cancellation.is_terminal()
                );
                retained.owner_cancellation.clone()
            } else {
                assert!(registry.lock_state().exchanges.is_empty());
                before.owner_cancellation.clone()
            };
            if foreign_owner {
                let retired = accept(&owner_binding).expect_err("owner observes real expiry");
                assert_eq!(retired.code, McpErrorCode::InvalidParams);
                assert_eq!(retired.message, MRTR_REQUEST_STATE_ERROR);
            }
            assert!(registry.lock_state().exchanges.is_empty());
            // After retirement, the preserved cancellation capability still
            // aliases the issuer's authority; equal flags alone cannot prove it.
            assert!(cancellation.cancel());
            assert!(retained_owner.is_cancel_requested());
            assert!(before.owner_cancellation.is_cancel_requested());
        }
    }

    #[test]
    fn auth_00_expired_mrtr_owner_retires_positive() {
        auth_00_expired_mrtr_owner_probe(false);
    }

    #[test]
    fn auth_00_expired_mrtr_foreign_owner_preserves_negative() {
        auth_00_expired_mrtr_owner_probe(true);
    }

    #[test]
    fn mrtr_state_only_retry_requires_absent_input_responses() {
        let registry = MrtrExchangeRegistry::new();
        let binding = MrtrExchangeBinding::new(
            "tools/call",
            "state-only-tool".to_owned(),
            [7; 32],
            [9; 32],
            None,
            [11; 32],
        );
        let required = registry
            .issue_bound(
                McpRequestCancellation::new(),
                binding.clone(),
                MrtrInputRequests::default(),
                None,
            )
            .expect("a state-only exchange issues");
        let wire = serde_json::to_value(&required).expect("state-only exchange serializes");
        assert!(
            wire.get("inputRequests").is_none(),
            "state-only input_required omits inputRequests"
        );
        let request_state = mrtr_state_from_wire(&required);

        let explicit_empty = registry
            .accept_wire_bound(&request_state, &binding, &BTreeMap::new())
            .expect_err("an explicit empty inputResponses map is not state-only");
        assert_eq!(explicit_empty.code, McpErrorCode::InvalidParams);
        assert_eq!(
            registry.active_len(),
            1,
            "the rejected explicit map leaves the state-only exchange available"
        );

        let completed = registry
            .accept_state_only_bound(&request_state, &binding)
            .expect("an absent inputResponses member completes the state-only exchange");
        let MrtrRetry::Complete(inputs) = completed else {
            panic!("state-only retry must complete without manufacturing inputs");
        };
        assert!(inputs.responses().is_empty());
        assert_eq!(registry.active_len(), 0);
    }

    fn mrtr_handler_state_probe(wrong_binding: bool) {
        let registry = MrtrExchangeRegistry::new();
        let binding = MrtrExchangeBinding::new(
            "tools/call",
            "shared-tool".to_owned(),
            [7; 32],
            [9; 32],
            None,
            [11; 32],
        );
        let mut foreign = binding.clone();
        foreign.session_partition = [10; 32];
        let mut states = Vec::new();
        for upstream_state in ["conversation-a", "conversation-b"] {
            let issued = registry
                .issue_bound(
                    McpRequestCancellation::new(),
                    binding.clone(),
                    MrtrInputRequests::new([
                        ("first".to_owned(), MrtrInputRequest::roots()),
                        ("second".to_owned(), MrtrInputRequest::roots()),
                    ])
                    .unwrap(),
                    Some(upstream_state.to_owned()),
                )
                .unwrap();
            let wire = serde_json::to_string(&issued).unwrap();
            assert!(
                !wire.contains(upstream_state),
                "upstream state stays private"
            );
            states.push(mrtr_state_from_wire(&issued));
        }
        assert_ne!(states[0], states[1]);
        assert_eq!(registry.active_len(), 2);
        // Rotate both conversations after a partial response, then complete
        // them in reverse order. Identical tool/arguments/input keys must not
        // collapse their distinct private state.
        for state in &mut states {
            let responses = BTreeMap::from([(
                "first".to_owned(),
                serde_json::to_value(mrtr_roots_response()).unwrap(),
            )]);
            if wrong_binding {
                let snapshot = || {
                    let locked = registry.lock_state();
                    let exchange = locked.exchanges.get(state).unwrap();
                    (
                        format!("{exchange:?}"),
                        exchange
                            .handler_request_state
                            .as_ref()
                            .map(|value| value.0.clone()),
                    )
                };
                let before = snapshot();
                let error = registry
                    .accept_wire_bound(state, &foreign, &responses)
                    .unwrap_err();
                assert_eq!(error.code, McpErrorCode::InvalidParams);
                assert_eq!(registry.active_len(), 2);
                assert_eq!(
                    snapshot(),
                    before,
                    "binding refusal preserves all retained fields and private state"
                );
            }
            let MrtrRetry::InputRequired(partial) = registry
                .accept_wire_bound(state, &binding, &responses)
                .unwrap()
            else {
                panic!("one outstanding response must remain");
            };
            assert!(
                registry
                    .accept_wire_bound(state, &binding, &responses)
                    .is_err()
            );
            *state = mrtr_state_from_wire(&partial);
        }
        for index in [1, 0] {
            let responses = BTreeMap::from([(
                "second".to_owned(),
                serde_json::to_value(mrtr_roots_response()).unwrap(),
            )]);
            let MrtrRetry::Complete(completed) = registry
                .accept_wire_bound(&states[index], &binding, &responses)
                .unwrap()
            else {
                panic!("both responses complete the exchange");
            };
            assert_eq!(
                completed.handler_request_state(),
                Some(["conversation-a", "conversation-b"][index])
            );
            assert!(
                !format!("{completed:?}").contains("conversation-"),
                "debug output must redact retained upstream state"
            );
            assert!(completed.roots("first").unwrap().is_some());
            assert!(completed.roots("second").unwrap().is_some());
            assert_eq!(registry.active_len(), index);
            assert!(
                registry
                    .accept_wire_bound(&states[index], &binding, &responses)
                    .is_err()
            );
        }
    }

    #[test]
    fn mrtr_handler_state_conversation_positive() {
        mrtr_handler_state_probe(false);
    }

    #[test]
    fn mrtr_handler_state_binding_planted_negative() {
        mrtr_handler_state_probe(true);
    }

    #[test]
    fn stateless_elicitation_state_only_retry_observes_cancellation_and_expiry_first() {
        let binding = MrtrExchangeBinding::stateless(
            "tools/call",
            "elicitation-tool".to_owned(),
            [17; 32],
            [19; 32],
            Some([23; 32]),
            [29; 32],
        );
        let input_requests = || {
            MrtrInputRequests::new([(
                "approval".to_owned(),
                MrtrInputRequest::final_elicitation(final_form_elicitation_params())
                    .expect("final elicitation request must encode"),
            )])
            .expect("unique final elicitation input map")
        };

        let cancelled_registry = MrtrExchangeRegistry::new();
        let owner = McpRequestCancellation::new();
        let cancelled = cancelled_registry
            .issue_bound(owner.clone(), binding.clone(), input_requests(), None)
            .expect("stateless elicitation state issues before owner cancellation");
        let cancelled_state = mrtr_state_from_wire(&cancelled);
        assert!(owner.cancel());
        let cancellation_error = cancelled_registry
            .accept_state_only_bound(&cancelled_state, &binding)
            .expect_err("owner cancellation must precede the stateless elicitation policy");
        assert_eq!(cancellation_error.code, McpErrorCode::RequestCancelled);
        assert_eq!(
            cancelled_registry.active_len(),
            0,
            "cancelled elicitation state must be removed"
        );

        let expired_registry = MrtrExchangeRegistry::with_limits(
            16,
            DEFAULT_MAX_MRTR_ROUNDS,
            DEFAULT_MAX_MRTR_INPUT_REQUESTS_PER_ROUND,
            DEFAULT_MAX_MRTR_INPUT_REQUESTS_TOTAL,
            Duration::from_millis(1),
        )
        .expect("bounded stateless elicitation registry");
        let issued_at = Instant::now();
        let expired = expired_registry
            .issue_at(
                McpRequestCancellation::new(),
                Some(binding.clone()),
                input_requests(),
                None,
                issued_at,
            )
            .expect("stateless elicitation state issues before expiry");
        let expired_state = mrtr_state_from_wire(&expired);
        let expiry_error = expired_registry
            .accept_at(
                &expired_state,
                Some(&binding),
                MrtrInputResponses::default(),
                true,
                issued_at + Duration::from_millis(1),
            )
            .expect_err("expiry must precede the stateless elicitation policy");
        assert_eq!(expiry_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            expired_registry.active_len(),
            0,
            "expired elicitation state must be removed"
        );
    }

    #[test]
    fn mrtr_expiry_and_owning_request_cancellation_prevent_resolution() {
        let registry = MrtrExchangeRegistry::with_limits(
            16,
            DEFAULT_MAX_MRTR_ROUNDS,
            DEFAULT_MAX_MRTR_INPUT_REQUESTS_PER_ROUND,
            DEFAULT_MAX_MRTR_INPUT_REQUESTS_TOTAL,
            Duration::from_millis(1),
        )
        .expect("bounded MRTR registry");
        let input_requests = || {
            MrtrInputRequests::new([("roots".to_owned(), MrtrInputRequest::roots())])
                .expect("unique MRTR input map")
        };

        let expired = registry
            .issue(McpRequestCancellation::new(), input_requests())
            .expect("MRTR input result must issue");
        let expired_state = mrtr_state_from_wire(&expired);
        let expiry_error = registry
            .accept_at(
                &expired_state,
                None,
                MrtrInputResponses::new([("roots".to_owned(), mrtr_roots_response())])
                    .expect("unique MRTR response map"),
                false,
                Instant::now() + Duration::from_millis(1),
            )
            .expect_err("expired state must fail before resolution");
        assert_eq!(expiry_error.code, McpErrorCode::InvalidParams);
        assert_eq!(registry.active_len(), 0, "expired state must be removed");

        let owner = McpRequestCancellation::new();
        let cancelled = registry
            .issue(owner.clone(), input_requests())
            .expect("MRTR input result must issue");
        let cancelled_state = mrtr_state_from_wire(&cancelled);
        assert!(owner.cancel());
        let cancellation_error = registry
            .accept(
                &cancelled_state,
                MrtrInputResponses::new([("roots".to_owned(), mrtr_roots_response())])
                    .expect("unique MRTR response map"),
            )
            .expect_err("owner cancellation must win before MRTR resolution");
        assert_eq!(cancellation_error.code, McpErrorCode::RequestCancelled);
        assert_eq!(registry.active_len(), 0, "cancelled state must be removed");
    }

    fn dual_era_boundary_with_recording_sender(
        era: ProtocolEra,
        sent_methods: Arc<Mutex<Vec<String>>>,
    ) -> DualEraServerToClient {
        let pending = Arc::new(PendingRequests::new());
        let pending_for_send = Arc::clone(&pending);
        let sent_methods_for_send = Arc::clone(&sent_methods);
        let send_fn: TransportSendFn = Arc::new(move |message| {
            let JsonRpcMessage::Request(request) = message else {
                panic!("the server-to-client boundary may only emit requests");
            };
            sent_methods_for_send
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.method.clone());

            let result = match request.method.as_str() {
                "sampling/createMessage" => serde_json::json!({
                    "content": {"type": "text", "text": "legacy completion"},
                    "role": "assistant",
                    "model": "legacy-model",
                    "stopReason": "endTurn"
                }),
                "elicitation/create" => serde_json::json!({"action": "decline"}),
                "roots/list" => serde_json::json!({"roots": []}),
                method => panic!("unexpected reverse JSON-RPC method: {method}"),
            };
            let id = request
                .id
                .clone()
                .expect("server-to-client requests require an ID");
            assert!(
                pending_for_send.route_response(&JsonRpcResponse::success(id, result)),
                "recorded reverse request must retain its response waiter"
            );
            Ok(())
        });

        DualEraServerToClient::new(
            era,
            RequestSender::new(pending, send_fn),
            Arc::new(MrtrExchangeRegistry::new()),
        )
    }

    #[test]
    fn dual_era_boundary_legacy_round_trips_the_three_exact_reverse_methods() {
        let sent_methods = Arc::new(Mutex::new(Vec::new()));
        let boundary = dual_era_boundary_with_recording_sender(
            ProtocolEra::Legacy2024,
            Arc::clone(&sent_methods),
        );
        let cx = Cx::for_testing();

        let sampling = block_on(boundary.sampling_create_message(
            &cx,
            McpRequestCancellation::new(),
            "sample",
            DualEraSamplingParams::Legacy2024(fastmcp_protocol::CreateMessageParams::new(
                Vec::new(),
                fastmcp_protocol::JsonInteger::from(16_i64),
            )),
        ))
        .expect("legacy sampling must await a direct response");
        let DualEraServerToClientResult::Legacy(sampling) = sampling else {
            panic!("legacy sampling must not create an MRTR retry");
        };
        assert_eq!(sampling.model, "legacy-model");

        let elicitation = block_on(boundary.elicitation_create(
            &cx,
            McpRequestCancellation::new(),
            "elicit",
            DualEraElicitationParams::Legacy2024(fastmcp_protocol::ElicitRequestParams::form(
                "Continue?",
                serde_json::json!({"type": "object"}),
            )),
        ))
        .expect("legacy elicitation must await a direct response");
        assert!(matches!(
            elicitation,
            DualEraServerToClientResult::Legacy(fastmcp_protocol::ElicitResult {
                action: fastmcp_protocol::ElicitAction::Decline,
                ..
            })
        ));

        let roots = block_on(boundary.roots_list(&cx, McpRequestCancellation::new(), "roots"))
            .expect("legacy roots must await a direct response");
        let DualEraServerToClientResult::Legacy(roots) = roots else {
            panic!("legacy roots must not create an MRTR retry");
        };
        assert_eq!(roots.roots.len(), 0);

        assert_eq!(boundary.era(), ProtocolEra::Legacy2024);
        assert_eq!(
            *sent_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec![
                "sampling/createMessage".to_owned(),
                "elicitation/create".to_owned(),
                "roots/list".to_owned(),
            ]
        );
    }

    #[test]
    fn dual_era_elicitation_keeps_legacy_url_identity_out_of_final_mrtr() {
        let pending = Arc::new(PendingRequests::new());
        let pending_for_send = Arc::clone(&pending);
        let sent_params = Arc::new(Mutex::new(Vec::new()));
        let sent_params_for_send = Arc::clone(&sent_params);
        let send_fn: TransportSendFn = Arc::new(move |message| {
            let JsonRpcMessage::Request(request) = message else {
                panic!("legacy isolation boundary may only emit requests");
            };
            assert_eq!(request.method, "elicitation/create");
            sent_params_for_send
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(
                    request
                        .params
                        .clone()
                        .expect("legacy elicitation carries params"),
                );
            let id = request
                .id
                .clone()
                .expect("legacy elicitation requires a JSON-RPC id");
            assert!(pending_for_send.route_response(&JsonRpcResponse::success(
                id,
                serde_json::json!({"action": "decline"}),
            )));
            Ok(())
        });
        let legacy = DualEraServerToClient::new(
            ProtocolEra::Legacy2024,
            RequestSender::new(pending, send_fn),
            Arc::new(MrtrExchangeRegistry::new()),
        );
        let cx = Cx::for_testing();

        block_on(legacy.elicitation_create(
            &cx,
            McpRequestCancellation::new(),
            "legacy-url",
            DualEraElicitationParams::Legacy2024(fastmcp_protocol::ElicitRequestParams::url(
                "Authorize legacy access",
                "https://example.com/legacy-authorize",
                "legacy-elicitation-id",
            )),
        ))
        .expect("exact-2024 URL elicitation must retain its identity");
        assert_eq!(
            sent_params
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_slice(),
            &[serde_json::json!({
                "mode": "url",
                "message": "Authorize legacy access",
                "url": "https://example.com/legacy-authorize",
                "elicitationId": "legacy-elicitation-id",
            })],
        );

        let legacy_error = block_on(legacy.elicitation_create(
            &cx,
            McpRequestCancellation::new(),
            "final-on-legacy",
            DualEraElicitationParams::Modern2026(final_url_elicitation_params()),
        ))
        .expect_err("a final descriptor must not cross into exact-2024 JSON-RPC");
        assert_eq!(legacy_error.code, McpErrorCode::InvalidParams);
        assert_eq!(
            sent_params
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "rejected final input must not reach the legacy sender",
        );

        let modern_registry = Arc::new(MrtrExchangeRegistry::new());
        let modern = DualEraServerToClient::Modern2026 {
            exchanges: Arc::clone(&modern_registry),
        };
        let modern_error = block_on(modern.elicitation_create(
            &cx,
            McpRequestCancellation::new(),
            "legacy-on-final",
            DualEraElicitationParams::Legacy2024(fastmcp_protocol::ElicitRequestParams::url(
                "Authorize legacy access",
                "https://example.com/legacy-authorize",
                "legacy-elicitation-id",
            )),
        ))
        .expect_err("a legacy descriptor must not mint final MRTR state");
        assert_eq!(modern_error.code, McpErrorCode::InvalidParams);
        assert_eq!(modern_registry.active_len(), 0);
    }

    fn assert_modern_elicitation_issues_and_reconstructs(
        input_key: &str,
        params: fastmcp_protocol::FinalEmbeddedElicitationParams,
        expected_params: serde_json::Value,
    ) {
        let registry = Arc::new(MrtrExchangeRegistry::new());
        let boundary = DualEraServerToClient::Modern2026 {
            exchanges: Arc::clone(&registry),
        };
        let cx = Cx::for_testing();
        let result = block_on(boundary.elicitation_create(
            &cx,
            McpRequestCancellation::new(),
            input_key,
            DualEraElicitationParams::Modern2026(params),
        ))
        .expect("final elicitation must issue MRTR state");
        let DualEraServerToClientResult::InputRequired(required) = result else {
            panic!("modern elicitation must issue a final input_required result");
        };
        assert_eq!(registry.active_len(), 1);
        let wire = serde_json::to_value(&required).expect("final input request serializes");
        let descriptor = wire["inputRequests"][input_key].clone();
        assert_eq!(descriptor["method"], "elicitation/create");
        assert_eq!(descriptor["params"], expected_params);

        let reconstructed = MrtrInputRequest::from_wire(&descriptor)
            .expect("final emitted descriptor must reconstruct without legacy conversion");
        assert_eq!(
            serde_json::to_value(reconstructed).expect("reconstructed descriptor serializes"),
            descriptor,
        );

        let complete = boundary
            .accept_input_retry(
                &cx,
                &mrtr_state_from_wire(&required),
                MrtrInputResponses::new([(
                    input_key.to_owned(),
                    MrtrInputResponse::elicitation(fastmcp_protocol::ElicitResult::decline())
                        .expect("elicitation response must serialize"),
                )])
                .expect("one matching final elicitation response"),
            )
            .expect("final elicitation retry must complete");
        let MrtrRetry::Complete(complete) = complete else {
            panic!("one matching elicitation response must complete the exchange");
        };
        assert_eq!(
            complete
                .responses()
                .get(input_key)
                .map(MrtrInputResponse::kind),
            Some(MrtrInputKind::Elicitation),
        );
    }

    #[test]
    fn dual_era_boundary_modern_issues_and_reconstructs_final_form_elicitation() {
        assert_modern_elicitation_issues_and_reconstructs(
            "form",
            final_form_elicitation_params(),
            serde_json::json!({
                "mode": "form",
                "message": "Choose a display name",
                "requestedSchema": {
                    "$schema": "https://json-schema.org/draft/2020-12/schema",
                    "type": "object",
                    "properties": {"displayName": {"type": "string", "minLength": 1}},
                    "required": ["displayName"],
                },
            }),
        );
    }

    #[test]
    fn dual_era_boundary_modern_issues_and_reconstructs_final_url_elicitation() {
        assert_modern_elicitation_issues_and_reconstructs(
            "url",
            final_url_elicitation_params(),
            serde_json::json!({
                "mode": "url",
                "message": "Authorize access",
                "url": "https://example.com/authorize",
            }),
        );
    }

    fn legacy_sampling_boundary_with_result(
        sampling_result: serde_json::Value,
    ) -> DualEraServerToClient {
        let pending = Arc::new(PendingRequests::new());
        let pending_for_send = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |message| {
            let JsonRpcMessage::Request(request) = message else {
                panic!("the legacy sampling boundary may only emit requests");
            };
            assert_eq!(request.method, "sampling/createMessage");
            let id = request
                .id
                .clone()
                .expect("legacy reverse sampling must retain its JSON-RPC ID");
            assert!(
                pending_for_send
                    .route_response(&JsonRpcResponse::success(id, sampling_result.clone())),
                "legacy sampling response must reach its registered waiter"
            );
            Ok(())
        });
        DualEraServerToClient::new(
            ProtocolEra::Legacy2024,
            RequestSender::new(pending, send_fn),
            Arc::new(MrtrExchangeRegistry::new()),
        )
    }

    #[test]
    fn dual_era_legacy_sampling_round_trips_an_absent_stop_reason() {
        let expected = serde_json::json!({
            "content": {"type": "text", "text": "legacy completion"},
            "role": "assistant",
            "model": "legacy-model"
        });
        let boundary = legacy_sampling_boundary_with_result(expected.clone());
        let cx = Cx::for_testing();

        let result = block_on(boundary.sampling_create_message(
            &cx,
            McpRequestCancellation::new(),
            "sample",
            DualEraSamplingParams::Legacy2024(fastmcp_protocol::CreateMessageParams::new(
                Vec::new(),
                fastmcp_protocol::JsonInteger::from(16_i64),
            )),
        ))
        .expect("legacy sampling must preserve an absent stopReason");
        let DualEraServerToClientResult::Legacy(result) = result else {
            panic!("legacy sampling must not create an MRTR retry");
        };

        assert_eq!(result.stop_reason, None);
        assert_eq!(
            serde_json::to_value(result).expect("legacy sampling result must re-encode"),
            expected
        );
    }

    #[test]
    fn dual_era_legacy_sampling_round_trips_an_open_provider_stop_reason() {
        let expected = serde_json::json!({
            "content": {"type": "text", "text": "legacy completion"},
            "role": "assistant",
            "model": "legacy-model",
            "stopReason": "provider_safety_limit"
        });
        let boundary = legacy_sampling_boundary_with_result(expected.clone());
        let cx = Cx::for_testing();

        let result = block_on(boundary.sampling_create_message(
            &cx,
            McpRequestCancellation::new(),
            "sample",
            DualEraSamplingParams::Legacy2024(fastmcp_protocol::CreateMessageParams::new(
                Vec::new(),
                fastmcp_protocol::JsonInteger::from(16_i64),
            )),
        ))
        .expect("legacy sampling must preserve an open provider stopReason");
        let DualEraServerToClientResult::Legacy(result) = result else {
            panic!("legacy sampling must not create an MRTR retry");
        };

        assert_eq!(result.stop_reason.as_deref(), Some("provider_safety_limit"));
        assert_eq!(
            serde_json::to_value(result).expect("legacy sampling result must re-encode"),
            expected
        );
    }

    #[test]
    fn dual_era_boundary_modern_uses_input_required_retry_flow() {
        let sent_methods = Arc::new(Mutex::new(Vec::new()));
        let boundary = dual_era_boundary_with_recording_sender(
            ProtocolEra::Modern2026,
            Arc::clone(&sent_methods),
        );
        let cx = Cx::for_testing();

        let sampling = block_on(boundary.sampling_create_message(
            &cx,
            McpRequestCancellation::new(),
            "sample",
            DualEraSamplingParams::Modern2026(final_sampling_params()),
        ))
        .expect("modern sampling must create an MRTR input result");
        let DualEraServerToClientResult::InputRequired(required) = sampling else {
            panic!("modern sampling must not send and await reverse JSON-RPC");
        };
        let wire = serde_json::to_value(&required).expect("MRTR result must serialize");
        assert_eq!(wire["resultType"], "input_required");
        assert_eq!(
            wire["inputRequests"]["sample"]["method"],
            "sampling/createMessage"
        );
        assert!(wire["inputRequests"]["sample"].get("jsonrpc").is_none());
        assert!(wire["inputRequests"]["sample"].get("id").is_none());

        let complete = boundary
            .accept_input_retry(
                &cx,
                &mrtr_state_from_wire(&required),
                MrtrInputResponses::new([(
                    "sample".to_owned(),
                    MrtrInputResponse::sampling(final_sampling_result())
                        .expect("sampling response must serialize"),
                )])
                .expect("one matching final response"),
            )
            .expect("modern retry must resolve through the MRTR registry");
        let MrtrRetry::Complete(complete) = complete else {
            panic!("a matching response must complete the one-input exchange");
        };
        assert_eq!(complete.responses().len(), 1);
        assert_eq!(
            complete
                .responses()
                .get("sample")
                .map(MrtrInputResponse::kind),
            Some(MrtrInputKind::Sampling)
        );
        assert_eq!(boundary.era(), ProtocolEra::Modern2026);
        assert!(
            sent_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "modern input_required must not emit a reverse JSON-RPC request"
        );
    }

    #[test]
    fn dual_era_boundary_modern_roots_never_sends_reverse_jsonrpc() {
        let sent_methods = Arc::new(Mutex::new(Vec::new()));
        let boundary = dual_era_boundary_with_recording_sender(
            ProtocolEra::Modern2026,
            Arc::clone(&sent_methods),
        );
        let cx = Cx::for_testing();

        let roots = block_on(boundary.roots_list(&cx, McpRequestCancellation::new(), "roots"))
            .expect("modern roots must create an MRTR input result");
        let DualEraServerToClientResult::InputRequired(required) = roots else {
            panic!("changing only the selected era must disable reverse roots/list");
        };
        let wire = serde_json::to_value(required).expect("MRTR result must serialize");
        assert_eq!(
            wire["inputRequests"]["roots"],
            serde_json::json!({"method": "roots/list"})
        );
        assert!(
            sent_methods
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .is_empty(),
            "MCP 2026-07-28 must not send roots/list as reverse JSON-RPC"
        );
    }

    #[test]
    fn test_pending_requests_register_and_route() {
        let pending = PendingRequests::new();

        // Register a request
        let (id, receiver) = pending.register().unwrap();

        // Simulate a response
        let response = JsonRpcResponse::success(id, serde_json::json!({"result": "ok"}));
        assert!(pending.route_response(&response));

        // Receive the response
        let result = receive_pending(receiver);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"result": "ok"}));
    }

    #[test]
    fn test_pending_requests_error_response() {
        let pending = PendingRequests::new();

        let (id, receiver) = pending.register().unwrap();

        // Simulate an error response
        let response = JsonRpcResponse::error(
            Some(id),
            JsonRpcError {
                code: (-32600).into(),
                message: "Invalid request".to_string(),
                data: None,
            },
        );
        assert!(pending.route_response(&response));

        // Receive the error
        let result = receive_pending(receiver);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().message, REMOTE_RESPONSE_ERROR);
    }

    #[test]
    fn test_pending_requests_cancel_all() {
        let pending = PendingRequests::new();

        let (_, receiver1) = pending.register().unwrap();
        let (_, receiver2) = pending.register().unwrap();

        // Cancel all
        pending.cancel_all();

        // Both should receive errors
        let result1 = receive_pending(receiver1);
        let result2 = receive_pending(receiver2);
        assert!(result1.is_err());
        assert!(result2.is_err());
    }

    #[test]
    fn request_local_cancellation_wakes_a_pending_bidirectional_wait() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct WakeFlag(AtomicBool);

        impl std::task::Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let pending = Arc::new(PendingRequests::new());
        let sent = Arc::new(AtomicBool::new(false));
        let sent_flag = Arc::clone(&sent);
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let outbound_for_send = Arc::clone(&outbound);
        let sender = RequestSender::new(
            Arc::clone(&pending),
            Arc::new(move |message| {
                sent_flag.store(true, Ordering::Release);
                outbound_for_send
                    .lock()
                    .expect("test outbound mutex must not be poisoned")
                    .push(message.clone());
                Ok(())
            }),
        );
        let cancellation = McpRequestCancellation::new();
        let scoped = sender.for_request(cancellation.clone());
        let cx = Cx::for_testing();
        let mut future = Box::pin(scoped.send_request::<serde_json::Value>(
            &cx,
            "test/request-local-cancellation",
            serde_json::json!({}),
        ));
        let wake_flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = std::task::Waker::from(Arc::clone(&wake_flag));
        let mut task_cx = std::task::Context::from_waker(&waker);

        assert!(std::future::Future::poll(future.as_mut(), &mut task_cx).is_pending());
        assert!(sent.load(Ordering::Acquire));
        assert_eq!(pending.in_flight_len(), 1);

        assert!(cancellation.cancel());
        assert!(wake_flag.0.load(Ordering::Acquire));
        let error = block_on(future).unwrap_err();
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(pending.in_flight_len(), 0);
        let outbound = outbound
            .lock()
            .expect("test outbound mutex must not be poisoned");
        assert_eq!(outbound.len(), 2);
        let JsonRpcMessage::Request(cancelled) = &outbound[1] else {
            panic!("cancelled reverse request must notify the peer");
        };
        assert_eq!(cancelled.method, "notifications/cancelled");
        assert_eq!(
            cancelled.params,
            Some(serde_json::json!({ "requestId": FIRST_SERVER_REQUEST_ID }))
        );
    }

    #[test]
    fn request_finalization_wakes_and_removes_a_pending_bidirectional_wait() {
        use std::sync::atomic::{AtomicBool, Ordering};

        struct WakeFlag(AtomicBool);

        impl std::task::Wake for WakeFlag {
            fn wake(self: Arc<Self>) {
                self.0.store(true, Ordering::Release);
            }
        }

        let pending = Arc::new(PendingRequests::new());
        let sender = RequestSender::new(Arc::clone(&pending), Arc::new(|_| Ok(())));
        let cancellation = McpRequestCancellation::new();
        let scoped = sender.for_request(cancellation.clone());
        let cx = Cx::for_testing();
        let mut future = Box::pin(scoped.send_request::<serde_json::Value>(
            &cx,
            "test/request-finalization",
            serde_json::json!({}),
        ));
        let wake_flag = Arc::new(WakeFlag(AtomicBool::new(false)));
        let waker = std::task::Waker::from(Arc::clone(&wake_flag));
        let mut task_cx = std::task::Context::from_waker(&waker);

        assert!(std::future::Future::poll(future.as_mut(), &mut task_cx).is_pending());
        assert_eq!(pending.in_flight_len(), 1);
        assert!(cancellation.begin_finalization());
        assert!(wake_flag.0.load(Ordering::Acquire));

        let error = block_on(future).expect_err("finalization must terminate the retained wait");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(pending.in_flight_len(), 0);
    }

    #[test]
    fn request_local_cancellation_wins_when_response_is_already_ready() {
        let pending = Arc::new(PendingRequests::new());
        let pending_for_send = Arc::clone(&pending);
        let cancellation = McpRequestCancellation::new();
        let cancellation_for_send = cancellation.clone();
        let sender = RequestSender::new(
            Arc::clone(&pending),
            Arc::new(move |message| {
                let JsonRpcMessage::Request(request) = message else {
                    return Err("expected request".to_string());
                };
                let id = request
                    .id
                    .clone()
                    .ok_or_else(|| "expected request id".to_string())?;
                let response = JsonRpcResponse::success(id, serde_json::json!({"ready": true}));
                if !pending_for_send.route_response(&response) {
                    return Err("response was not routed".to_string());
                }
                let _ = cancellation_for_send.cancel();
                Ok(())
            }),
        )
        .for_request(cancellation);
        let cx = Cx::for_testing();

        let error = block_on(sender.send_request::<serde_json::Value>(
            &cx,
            "test/cancellation-precedence",
            serde_json::json!({}),
        ))
        .expect_err("request-local cancellation must own an observable tie");

        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert_eq!(pending.in_flight_len(), 0);
        assert!(!cx.is_cancel_requested());
    }

    #[test]
    fn test_route_unknown_response() {
        let pending = PendingRequests::new();

        // Route a response with unknown ID
        let response = JsonRpcResponse::success(
            RequestId::Number(999999),
            serde_json::json!({"result": "ok"}),
        );
        assert!(!pending.route_response(&response));
    }

    #[test]
    fn exact_legacy_negative_response_disposition_delivers_issued_waiter() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(1).unwrap();
        let (id, receiver) = pending.register().unwrap();
        assert_eq!(id, RequestId::Number(-1));

        let RequestId::Number(first_emitted_id) = id.clone() else {
            panic!("exact-legacy IDs must be numeric");
        };
        #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
        let f64_round_trip = (first_emitted_id as f64) as i64;
        assert_eq!(f64_round_trip, first_emitted_id);

        let response = JsonRpcResponse::success(id, serde_json::json!({"result": "ok"}));
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::Delivered
        );
        assert_eq!(
            receive_pending(receiver).unwrap(),
            serde_json::json!({"result": "ok"})
        );
    }

    #[test]
    fn exact_legacy_negative_ids_descend_from_minus_one() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(2).unwrap();

        let (first_id, _first_receiver) = pending.register().unwrap();
        let (next_id, _next_receiver) = pending.register().unwrap();

        assert_eq!(first_id, RequestId::Number(-1));
        assert_eq!(next_id, RequestId::Number(-2));
    }

    #[test]
    fn exact_legacy_negative_response_disposition_retires_issued_removed_id() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(1).unwrap();
        let (id, _receiver) = pending.register().unwrap();
        pending.remove(&id);

        let response = JsonRpcResponse::success(id, serde_json::json!(null));
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::RetiredGeneric
        );
        assert!(
            !pending.route_response(&response),
            "the public bool wrapper remains false for a retired response"
        );
    }

    #[test]
    fn exact_legacy_negative_response_disposition_rejects_unissued_nearby_id() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(1).unwrap();
        let (issued_id, _receiver) = pending.register().unwrap();
        assert_eq!(issued_id, RequestId::Number(-1));

        let unissued_id = RequestId::Number(-2);
        let response = JsonRpcResponse::success(unissued_id, serde_json::json!(null));
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::Unmatched
        );
        assert!(
            !pending.route_response(&response),
            "the public bool wrapper remains false for an unissued response"
        );
    }

    #[test]
    fn pending_requests_deliver_equivalent_numeric_response_spelling() {
        let pending = PendingRequests::new();
        let (id, receiver) = pending.register().unwrap();
        assert_eq!(id, RequestId::Number(FIRST_SERVER_REQUEST_ID));

        let response = JsonRpcResponse::success(
            RequestId::Integer(format!("{FIRST_SERVER_REQUEST_ID}e0")),
            serde_json::json!({"result": "canonical"}),
        );
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::Delivered
        );
        assert_eq!(
            receive_pending(receiver).unwrap(),
            serde_json::json!({"result": "canonical"})
        );
    }

    #[test]
    fn exact_legacy_retires_equivalent_numeric_response_spelling() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(1).unwrap();
        let (id, _receiver) = pending.register().unwrap();
        assert_eq!(id, RequestId::Number(-1));
        pending.remove(&id);

        let response = JsonRpcResponse::success(
            RequestId::Integer(format!("{FIRST_EXACT_LEGACY_SERVER_REQUEST_ID}e0")),
            serde_json::json!(null),
        );
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::RetiredGeneric
        );
    }

    #[test]
    fn removed_positive_id_remains_unmatched() {
        let pending = PendingRequests::new();
        let (id, _receiver) = pending.register().unwrap();
        pending.remove(&id);

        let response = JsonRpcResponse::success(id, serde_json::json!(null));
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::Unmatched
        );
        assert!(!pending.route_response(&response));
    }

    #[test]
    fn exact_legacy_negative_ids_exhaust_at_js_safe_boundary_and_remain_retired() {
        let pending = PendingRequests::with_max_in_flight_for_exact_legacy(1).unwrap();
        pending.set_next_id_for_test(LAST_EXACT_LEGACY_SERVER_REQUEST_ID);
        let (last_id, _receiver) = pending.register().unwrap();
        assert_eq!(
            last_id,
            RequestId::Number(LAST_EXACT_LEGACY_SERVER_REQUEST_ID)
        );
        pending.remove(&last_id);

        let exhausted = pending
            .register()
            .expect_err("the exact-legacy negative ID domain ends at the JavaScript safe boundary");
        assert_eq!(exhausted.message, REQUEST_ID_EXHAUSTED_ERROR);

        let response = JsonRpcResponse::success(last_id.clone(), serde_json::json!(null));
        assert_eq!(
            pending.route_response_with_disposition(&response),
            PendingResponseDisposition::RetiredGeneric
        );

        let first_response =
            JsonRpcResponse::success(RequestId::Number(-1), serde_json::json!(null));
        assert_eq!(
            pending.route_response_with_disposition(&first_response),
            PendingResponseDisposition::RetiredGeneric,
            "after exhaustion the entire JavaScript-safe negative range is retired"
        );

        let out_of_range_response = JsonRpcResponse::success(
            RequestId::Number(LAST_EXACT_LEGACY_SERVER_REQUEST_ID - 1),
            serde_json::json!(null),
        );
        assert_eq!(
            pending.route_response_with_disposition(&out_of_range_response),
            PendingResponseDisposition::Unmatched,
            "a negative ID outside the JavaScript-safe range is never retired"
        );

        let permanently_exhausted = pending
            .register()
            .expect_err("retiring the final negative ID must not permit reuse");
        assert_eq!(permanently_exhausted.message, REQUEST_ID_EXHAUSTED_ERROR);
    }

    // ── PendingRequests additional coverage ───────────────────────────

    #[test]
    fn pending_requests_default_is_same_as_new() {
        let pr = PendingRequests::default();
        let (id, _receiver) = pr.register().unwrap();
        // IDs start at 1_000_000
        assert_eq!(id, RequestId::Number(1_000_000));
        assert_eq!(pr.max_in_flight(), DEFAULT_MAX_IN_FLIGHT_REQUESTS);
    }

    #[test]
    fn pending_requests_ids_are_sequential() {
        let pr = PendingRequests::new();
        let (id1, _receiver1) = pr.register().unwrap();
        let (id2, _receiver2) = pr.register().unwrap();
        let (id3, _receiver3) = pr.register().unwrap();
        assert_eq!(id1, RequestId::Number(1_000_000));
        assert_eq!(id2, RequestId::Number(1_000_001));
        assert_eq!(id3, RequestId::Number(1_000_002));
    }

    #[test]
    fn pending_requests_limit_configuration_has_exact_hard_boundary() {
        let at_hard_limit =
            PendingRequests::with_max_in_flight(HARD_MAX_IN_FLIGHT_REQUESTS).unwrap();
        assert_eq!(at_hard_limit.max_in_flight(), HARD_MAX_IN_FLIGHT_REQUESTS);

        for invalid in [0, HARD_MAX_IN_FLIGHT_REQUESTS + 1] {
            let error = PendingRequests::with_max_in_flight(invalid).unwrap_err();
            assert_eq!(error.code, McpErrorCode::InvalidParams);
            assert_eq!(error.message, INVALID_LIMIT_ERROR);
        }
    }

    #[test]
    fn pending_requests_enforces_exact_in_flight_boundary_and_recovers_capacity() {
        let pr = PendingRequests::with_max_in_flight(2).unwrap();
        let (id1, receiver1) = pr.register().unwrap();
        let (_id2, _receiver2) = pr.register().unwrap();
        assert_eq!(pr.in_flight_len(), 2);

        let error = pr.register().unwrap_err();
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(error.message, IN_FLIGHT_LIMIT_ERROR);

        let response = JsonRpcResponse::success(id1, serde_json::json!(1));
        assert!(pr.route_response(&response));
        assert_eq!(receive_pending(receiver1).unwrap(), serde_json::json!(1));
        assert_eq!(pr.in_flight_len(), 1);

        let (_id3, _receiver3) = pr.register().unwrap();
        assert_eq!(pr.in_flight_len(), 2);
    }

    #[test]
    fn pending_request_ids_fail_closed_before_wrap_or_reuse() {
        let pr = PendingRequests::with_max_in_flight(4).unwrap();
        pr.set_next_id_for_test(i64::MAX);
        let (max_id, _max_receiver) = pr.register().unwrap();
        assert_eq!(max_id, RequestId::Number(i64::MAX));

        let exhausted = pr
            .register()
            .expect_err("request IDs must never wrap back to an earlier value");
        assert_eq!(exhausted.message, REQUEST_ID_EXHAUSTED_ERROR);
        assert_eq!(pr.in_flight_len(), 1);

        pr.remove(&max_id);
        let still_exhausted = pr
            .register()
            .expect_err("exhaustion must remain permanent after the last waiter leaves");
        assert_eq!(still_exhausted.message, REQUEST_ID_EXHAUSTED_ERROR);
    }

    #[test]
    fn pending_requests_remove_prevents_routing() {
        let pr = PendingRequests::new();
        let (id, _receiver) = pr.register().unwrap();

        // Remove the pending request
        pr.remove(&id);

        // Routing should fail now
        let response = JsonRpcResponse::success(id, serde_json::json!(null));
        assert_eq!(
            pr.route_response_with_disposition(&response),
            PendingResponseDisposition::Unmatched
        );
        assert!(!pr.route_response(&response));
    }

    #[test]
    fn pending_requests_route_response_without_id_returns_false() {
        let pr = PendingRequests::new();
        let (id, receiver) = pr.register().unwrap();
        // A response with no id
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed("2.0"),
            id: None,
            result: Some(serde_json::json!(null)),
            error: None,
        };
        assert!(!pr.route_response(&response));
        assert_eq!(pr.in_flight_len(), 1);

        let response = JsonRpcResponse::success(id, serde_json::json!(42));
        assert!(pr.route_response(&response));
        assert_eq!(receive_pending(receiver).unwrap(), serde_json::json!(42));
    }

    #[test]
    fn pending_requests_route_response_with_explicit_null_result() {
        let pr = PendingRequests::new();
        let (id, receiver) = pr.register().unwrap();

        // An explicit JSON null is a present and valid success result.
        let response = JsonRpcResponse {
            jsonrpc: std::borrow::Cow::Borrowed("2.0"),
            id: Some(id),
            result: Some(serde_json::Value::Null),
            error: None,
        };
        assert!(pr.route_response(&response));

        let result = receive_pending(receiver).unwrap();
        assert_eq!(result, serde_json::Value::Null);
    }

    #[test]
    fn pending_requests_rejects_invalid_response_shapes_and_versions() {
        let cases = [
            (
                Some(serde_json::Value::Null),
                Some(JsonRpcError {
                    code: (-32_603).into(),
                    message: "secret both-member detail".to_string(),
                    data: Some(serde_json::json!({"secret": true})),
                }),
                "2.0",
            ),
            (None, None, "2.0"),
            (Some(serde_json::Value::Null), None, "1.0"),
        ];

        for (result, error, version) in cases {
            let pr = PendingRequests::new();
            let (id, receiver) = pr.register().unwrap();
            let response = JsonRpcResponse {
                jsonrpc: std::borrow::Cow::Borrowed(version),
                result,
                error,
                id: Some(id),
            };

            assert!(pr.route_response(&response));
            let error = receive_pending(receiver).unwrap_err();
            assert_eq!(error.code, McpErrorCode::InternalError);
            assert_eq!(error.message, INVALID_RESPONSE_ERROR);
            assert!(error.data.is_none());
            assert_eq!(pr.in_flight_len(), 0);
        }
    }

    #[test]
    fn pending_requests_route_after_receiver_dropped_does_not_panic() {
        let pr = PendingRequests::new();
        let (id, receiver) = pr.register().unwrap();

        // Drop the receiver
        drop(receiver);

        // Routing should still succeed (sender.send returns Err but is ignored)
        let response = JsonRpcResponse::success(id, serde_json::json!(42));
        assert!(pr.route_response(&response));
    }

    #[test]
    fn pending_requests_cancel_all_clears_pending() {
        let pr = PendingRequests::new();
        let (id, _receiver) = pr.register().unwrap();

        pr.cancel_all();

        // No more pending requests to route to
        let response = JsonRpcResponse::success(id, serde_json::json!(null));
        assert!(!pr.route_response(&response));
    }

    #[test]
    fn pending_requests_cancel_all_empty_is_noop() {
        let pr = PendingRequests::new();
        // Should not panic on empty
        pr.cancel_all();
    }

    #[test]
    fn pending_requests_cancel_all_permanently_rejects_new_waiters() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let pending = Arc::new(PendingRequests::new());
        let (_, receiver) = pending.register().unwrap();

        pending.cancel_all();
        pending.cancel_all();

        let cancelled = receive_pending(receiver).unwrap_err();
        assert_eq!(cancelled.code, McpErrorCode::InternalError);
        assert_eq!(cancelled.message, CONNECTION_CLOSED_ERROR);
        assert_eq!(pending.in_flight_len(), 0);

        let registration_error = pending.register().unwrap_err();
        assert_eq!(registration_error.code, McpErrorCode::InternalError);
        assert_eq!(registration_error.message, CONNECTION_CLOSED_ERROR);

        let send_called = Arc::new(AtomicBool::new(false));
        let send_called_for_callback = Arc::clone(&send_called);
        let sender = RequestSender::new(
            Arc::clone(&pending),
            Arc::new(move |_| {
                send_called_for_callback.store(true, Ordering::Release);
                Ok(())
            }),
        );
        let cx = Cx::for_testing();
        let error = block_on(sender.send_request::<serde_json::Value>(
            &cx,
            "test/after-close",
            serde_json::json!({}),
        ))
        .unwrap_err();
        assert_eq!(error.code, McpErrorCode::InternalError);
        assert_eq!(error.message, CONNECTION_CLOSED_ERROR);
        assert!(!send_called.load(Ordering::Acquire));
    }

    #[test]
    fn pending_requests_debug_format() {
        let pr = PendingRequests::new();
        let debug = format!("{:?}", pr);
        assert!(debug.contains("PendingRequests"));
    }

    // ── RequestSender ────────────────────────────────────────────────

    #[test]
    fn request_sender_debug_format() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(pending, send_fn);
        let debug = format!("{:?}", sender);
        assert!(debug.contains("RequestSender"));
    }

    #[test]
    fn request_sender_transport_failure_returns_error() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Err("transport down".to_string()));
        let sender = RequestSender::new(pending, send_fn);

        let cx = Cx::for_testing();
        let result: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));
        let err = result.unwrap_err();
        assert_eq!(err.message, TRANSPORT_SEND_ERROR);
        assert!(!err.message.contains("transport down"));
    }

    #[test]
    fn request_sender_transport_failure_cleans_up_pending() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Err("fail".to_string()));
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);

        let cx = Cx::for_testing();
        let _error: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));

        // The pending request should have been cleaned up
        let id = RequestId::Number(1_000_000); // first ID
        let response = JsonRpcResponse::success(id, serde_json::json!(null));
        assert!(!pending.route_response(&response));
    }

    #[test]
    fn request_sender_clone() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(pending, send_fn);
        let cloned = sender.clone();
        let debug = format!("{:?}", cloned);
        assert!(debug.contains("RequestSender"));
    }

    #[test]
    fn dropping_request_future_releases_in_flight_capacity() {
        let pending = Arc::new(PendingRequests::with_max_in_flight(1).unwrap());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        {
            let mut future = Box::pin(sender.send_request::<serde_json::Value>(
                &cx,
                "test/method",
                serde_json::json!({}),
            ));
            let waker = std::task::Waker::noop();
            let mut task_cx = std::task::Context::from_waker(waker);

            assert!(std::future::Future::poll(future.as_mut(), &mut task_cx).is_pending());
            assert_eq!(pending.in_flight_len(), 1);
        }
        assert_eq!(pending.in_flight_len(), 0);
    }

    // ── RequestSender send_request paths ─────────────────────────────

    #[test]
    fn reverse_request_routes_matching_response_without_cancellation_cleanup() {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let outbound_for_send = Arc::clone(&outbound);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                outbound_for_send
                    .lock()
                    .expect("test outbound mutex must not be poisoned")
                    .push(msg.clone());
                let id = req.id.clone().unwrap();
                let response = JsonRpcResponse::success(id, serde_json::json!({"answer": 42}));
                assert!(pending_clone.route_response(&response));
            }
            Ok(())
        });
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        let result: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));
        let value = result.unwrap();
        assert_eq!(value["answer"], 42);
        assert_eq!(pending.in_flight_len(), 0);
        assert_eq!(
            outbound
                .lock()
                .expect("test outbound mutex must not be poisoned")
                .len(),
            1
        );
    }

    fn dropped_reverse_request_outbound(era: ProtocolEra) -> Vec<JsonRpcMessage> {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let outbound = Arc::new(Mutex::new(Vec::new()));
        let outbound_for_send = Arc::clone(&outbound);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                outbound_for_send
                    .lock()
                    .expect("test outbound mutex must not be poisoned")
                    .push(msg.clone());
                if let Some(RequestId::Number(id)) = req.id.as_ref() {
                    // RH-5 planted negative: only the response correlation ID
                    // differs from the successful reverse-request path above.
                    let response = JsonRpcResponse::success(
                        RequestId::Number(*id + 1),
                        serde_json::json!({"answer": 42}),
                    );
                    assert!(!pending_clone.route_response(&response));
                }
            }
            Ok(())
        });
        let sender = RequestSender::new_for_era(era, Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();

        {
            let mut future = Box::pin(sender.send_request::<serde_json::Value>(
                &cx,
                "test/method",
                serde_json::json!({}),
            ));
            let waker = std::task::Waker::noop();
            let mut task_cx = std::task::Context::from_waker(waker);

            assert!(std::future::Future::poll(future.as_mut(), &mut task_cx).is_pending());
            assert_eq!(pending.in_flight_len(), 1);
        }

        assert_eq!(pending.in_flight_len(), 0);
        outbound
            .lock()
            .expect("test outbound mutex must not be poisoned")
            .clone()
    }

    #[test]
    fn dropped_legacy_reverse_request_emits_cancellation_control() {
        let outbound = dropped_reverse_request_outbound(ProtocolEra::Legacy2024);
        assert_eq!(outbound.len(), 2);
        let JsonRpcMessage::Request(cancelled) = &outbound[1] else {
            panic!("dropped exact-2024 reverse request must notify the peer");
        };
        assert_eq!(cancelled.id, None);
        assert_eq!(cancelled.method, "notifications/cancelled");
        assert_eq!(
            cancelled.params,
            Some(serde_json::json!({ "requestId": FIRST_SERVER_REQUEST_ID }))
        );
    }

    #[test]
    fn dropped_modern_reverse_request_omits_cancellation_control() {
        // RH-5 planted negative: only the selected protocol era differs from
        // the legacy positive above.
        let outbound = dropped_reverse_request_outbound(ProtocolEra::Modern2026);
        assert_eq!(outbound.len(), 1);
        let JsonRpcMessage::Request(request) = &outbound[0] else {
            panic!("dropped modern reverse request must retain its initial request frame");
        };
        assert_eq!(request.method, "test/method");
        assert_eq!(request.id, Some(RequestId::Number(FIRST_SERVER_REQUEST_ID)));
    }

    #[test]
    fn request_sender_error_response_path() {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                let id = req.id.clone().unwrap();
                let response = JsonRpcResponse::error(
                    Some(id),
                    JsonRpcError {
                        code: (-32600).into(),
                        message: "bad request".to_string(),
                        data: None,
                    },
                );
                pending_clone.route_response(&response);
            }
            Ok(())
        });
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        let result: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));
        let err = result.unwrap_err();
        assert_eq!(err.message, REMOTE_RESPONSE_ERROR);
        assert!(!err.message.contains("bad request"));
    }

    #[test]
    fn request_sender_disconnected_path() {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                let id = req.id.clone().unwrap();
                // Remove the pending entry so tx is dropped, causing Disconnected
                pending_clone.remove(&id);
            }
            Ok(())
        });
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        let result: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));
        let err = result.unwrap_err();
        assert_eq!(err.message, RESPONSE_CHANNEL_ERROR);
    }

    #[test]
    fn request_sender_deserialization_error() {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                let id = req.id.clone().unwrap();
                // Return a string value, which won't deserialize to Vec<String>
                let response =
                    JsonRpcResponse::success(id, serde_json::json!("not a vec of strings"));
                pending_clone.route_response(&response);
            }
            Ok(())
        });
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        let result: McpResult<Vec<String>> =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({})));
        let err = result.unwrap_err();
        assert_eq!(err.message, RESPONSE_PAYLOAD_ERROR);
        assert!(!err.message.contains("expected"));
    }

    // ── cancel_all error details ─────────────────────────────────────

    #[test]
    fn cancel_all_sends_connection_closed_error() {
        let pr = PendingRequests::new();
        let (_, receiver) = pr.register().unwrap();
        pr.cancel_all();
        let result = receive_pending(receiver);
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::InternalError);
        assert_eq!(err.message, CONNECTION_CLOSED_ERROR);
        assert!(err.data.is_none());
    }

    // ── route_response with error containing data ────────────────────

    #[test]
    fn route_response_error_with_data() {
        let pr = PendingRequests::new();
        let (id, receiver) = pr.register().unwrap();
        let response = JsonRpcResponse::error(
            Some(id),
            JsonRpcError {
                code: (-32001).into(),
                message: "custom error".to_string(),
                data: Some(serde_json::json!({"detail": "extra info"})),
            },
        );
        assert!(pr.route_response(&response));
        let result = receive_pending(receiver);
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::ResourceNotFound);
        assert_eq!(err.message, REMOTE_RESPONSE_ERROR);
        assert!(err.data.is_none());
    }

    // ── Multiple concurrent register/route ───────────────────────────

    #[test]
    fn pending_requests_multiple_register_and_route_independently() {
        let pr = PendingRequests::new();
        let (id1, rx1) = pr.register().unwrap();
        let (id2, rx2) = pr.register().unwrap();
        let (id3, rx3) = pr.register().unwrap();

        // Route them out of order
        let r2 = JsonRpcResponse::success(id2.clone(), serde_json::json!("second"));
        let r3 = JsonRpcResponse::success(id3.clone(), serde_json::json!("third"));
        let r1 = JsonRpcResponse::success(id1.clone(), serde_json::json!("first"));
        assert!(pr.route_response(&r2));
        assert!(pr.route_response(&r3));
        assert!(pr.route_response(&r1));

        assert_eq!(receive_pending(rx1).unwrap(), serde_json::json!("first"));
        assert_eq!(receive_pending(rx2).unwrap(), serde_json::json!("second"));
        assert_eq!(receive_pending(rx3).unwrap(), serde_json::json!("third"));
    }

    #[test]
    fn pending_request_trackers_isolate_identical_wire_ids() {
        let connection_a = PendingRequests::new();
        let connection_b = PendingRequests::new();
        let (id_a, receiver_a) = connection_a.register().unwrap();
        let (id_b, receiver_b) = connection_b.register().unwrap();
        assert_eq!(id_a, id_b);

        let response = JsonRpcResponse::success(id_b, serde_json::json!("connection-b"));
        assert!(connection_b.route_response(&response));
        assert_eq!(
            receive_pending(receiver_b).unwrap(),
            serde_json::json!("connection-b")
        );
        assert_eq!(connection_b.in_flight_len(), 0);

        // Routing on B cannot consume A's same-numbered waiter because the
        // registry itself is the immutable connection ownership boundary.
        assert_eq!(connection_a.in_flight_len(), 1);
        connection_a.cancel_all();
        let error = receive_pending(receiver_a).unwrap_err();
        assert_eq!(error.message, CONNECTION_CLOSED_ERROR);
    }

    // ── Transport sender constructors ────────────────────────────────

    #[test]
    fn transport_sampling_sender_new_and_clone() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(pending, send_fn);
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let _cloned = sampling.clone();
    }

    #[test]
    fn transport_elicitation_sender_new_and_clone() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(pending, send_fn);
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let _cloned = elicitation.clone();
    }

    #[test]
    fn transport_roots_provider_new_and_clone() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(pending, send_fn);
        let roots = TransportRootsProvider::new(sender, McpContext::new(Cx::for_testing(), 0));
        let _cloned = roots.clone();
    }

    // ── lock_state with poisoned mutex ───────────────────────────────

    #[test]
    fn pending_requests_lock_state_recovers_from_poison() {
        let pr = Arc::new(PendingRequests::new());
        let (id, receiver) = pr.register().unwrap();

        // Poison the mutex by panicking while holding the lock
        let pr2 = Arc::clone(&pr);
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = pr2.state.lock().unwrap();
            panic!("intentional poison");
        }));

        // lock_state should recover from poison (into_inner)
        // Routing should still work
        let response = JsonRpcResponse::success(id, serde_json::json!("recovered"));
        assert!(pr.route_response(&response));
        let result = receive_pending(receiver).unwrap();
        assert_eq!(result, serde_json::json!("recovered"));
    }

    // ── TransportSamplingSender — create_message ─────────────────────

    fn make_sender_with_responder(
        responder: impl Fn(&JsonRpcRequest) -> serde_json::Value + Send + Sync + 'static,
    ) -> RequestSender {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                let id = req.id.clone().unwrap();
                let result = responder(req);
                let response = JsonRpcResponse::success(id, result);
                pending_clone.route_response(&response);
            }
            Ok(())
        });
        RequestSender::new(pending, send_fn)
    }

    #[test]
    fn transport_sampling_sender_create_message_text() {
        let sender = make_sender_with_responder(|request| {
            let params = request
                .params
                .as_ref()
                .expect("sampling request must retain parameters");
            assert!(
                params.get("metadata").is_none(),
                "the transport must omit unspecified provider metadata"
            );
            serde_json::json!({
                "content": {"type": "text", "text": "Hello world"},
                "role": "assistant",
                "model": "test-model",
                "stopReason": "endTurn"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![fastmcp_core::SamplingRequestMessage {
                role: SamplingRole::User,
                text: "Hi".to_string(),
            }],
            max_tokens: 100,
            system_prompt: Some("Be helpful".to_string()),
            temperature: Some(0.7),
            stop_sequences: vec!["STOP".to_string()],
            model_hints: vec![],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert_eq!(result.text, "Hello world");
        assert_eq!(result.model, "test-model");
        assert!(matches!(result.stop_reason, SamplingStopReason::EndTurn));
    }

    #[test]
    fn transport_sampling_sender_round_trips_open_legacy_stop_reason_through_callback() {
        let expected = serde_json::json!({
            "content": {"type": "text", "text": "legacy completion"},
            "role": "assistant",
            "model": "legacy-model",
            "stopReason": "provider_safety_limit"
        });
        let reply = expected.clone();
        let sender = make_sender_with_responder(move |request| {
            assert_eq!(request.method, "sampling/createMessage");
            reply.clone()
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let callback_response = fastmcp_core::block_on(SamplingSender::create_message(
            &sampling,
            SamplingRequest::prompt("Hi", 10),
        ))
        .expect("legacy sampling callback must retain an open stopReason");
        assert_eq!(
            callback_response.stop_reason,
            SamplingStopReason::Other("provider_safety_limit".to_owned())
        );

        let emitted = fastmcp_protocol::CreateMessageResult {
            content: fastmcp_protocol::SamplingContent::Text {
                text: callback_response.text,
            },
            role: fastmcp_protocol::Role::Assistant,
            model: callback_response.model,
            stop_reason: callback_response
                .stop_reason
                .as_wire_value()
                .map(str::to_owned),
            meta: None,
        };
        let emitted = serde_json::to_value(emitted)
            .expect("legacy sampling callback response must serialize");
        assert_eq!(emitted, expected);
        assert!(emitted.get("resultType").is_none());
    }

    #[test]
    fn transport_sampling_sender_create_message_image() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "content": {"type": "image", "data": "aW1hZ2VkYXRh", "mimeType": "image/png"},
                "role": "assistant",
                "model": "vision-model",
                "stopReason": "maxTokens"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![fastmcp_core::SamplingRequestMessage {
                role: SamplingRole::User,
                text: "Describe image".to_string(),
            }],
            max_tokens: 50,
            system_prompt: None,
            temperature: None,
            stop_sequences: vec![],
            model_hints: vec![],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future).unwrap();
        // Image content is formatted as "[image: N bytes, type: ...]"
        assert!(result.text.contains("image"));
        assert!(result.text.contains("image/png"));
        assert_eq!(result.model, "vision-model");
        assert!(matches!(result.stop_reason, SamplingStopReason::MaxTokens));
    }

    #[test]
    fn transport_sampling_sender_create_message_with_model_hints() {
        let sender = make_sender_with_responder(|req| {
            // Verify model_preferences was sent
            let params: serde_json::Value =
                serde_json::from_value(req.params.clone().unwrap()).unwrap();
            assert!(params["modelPreferences"]["hints"].is_array());
            serde_json::json!({
                "content": {"type": "text", "text": "ok"},
                "role": "assistant",
                "model": "preferred",
                "stopReason": "stopSequence"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![fastmcp_core::SamplingRequestMessage {
                role: SamplingRole::User,
                text: "Hi".to_string(),
            }],
            max_tokens: 10,
            system_prompt: None,
            temperature: None,
            stop_sequences: vec![],
            model_hints: vec!["claude-3".to_string()],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert!(matches!(
            result.stop_reason,
            SamplingStopReason::StopSequence
        ));
    }

    #[test]
    fn transport_sampling_sender_create_message_assistant_role() {
        let sender = make_sender_with_responder(|req| {
            let params: serde_json::Value =
                serde_json::from_value(req.params.clone().unwrap()).unwrap();
            assert_eq!(params["messages"][0]["role"], "assistant");
            serde_json::json!({
                "content": {"type": "text", "text": "continued"},
                "role": "assistant",
                "model": "m",
                "stopReason": "endTurn"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![fastmcp_core::SamplingRequestMessage {
                role: SamplingRole::Assistant,
                text: "Previous response".to_string(),
            }],
            max_tokens: 10,
            system_prompt: None,
            temperature: None,
            stop_sequences: vec![],
            model_hints: vec![],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert_eq!(result.text, "continued");
    }

    #[test]
    fn transport_sampling_sender_rejects_non_assistant_result_role() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "content": {"type": "text", "text": "not authoritative"},
                "role": "user",
                "model": "m",
                "stopReason": "endTurn"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let request = SamplingRequest::prompt("Hi", 10);

        let error = fastmcp_core::block_on(SamplingSender::create_message(&sampling, request))
            .expect_err("sampling results must retain the documented assistant role");

        assert_eq!(error.message, RESPONSE_PAYLOAD_ERROR);
    }

    // ── TransportElicitationSender — elicit ──────────────────────────

    #[test]
    fn transport_elicitation_sender_form_accept_with_content() {
        let sender = make_sender_with_responder(|req| {
            let params: serde_json::Value =
                serde_json::from_value(req.params.clone().unwrap()).unwrap();
            assert_eq!(params["mode"], "form");
            serde_json::json!({
                "action": "accept",
                "content": {
                    "name": "Alice",
                    "age": 30,
                    "active": true,
                    "score": 9.5,
                    "tags": ["a", "b"],
                    "empty": null
                }
            })
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = ElicitationRequest {
            message: "Fill the form".to_string(),
            mode: ElicitationMode::Form,
            schema: Some(serde_json::json!({"type": "object"})),
            url: None,
            elicitation_id: None,
        };

        let future = ElicitationSender::elicit(&elicitation, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert!(matches!(result.action, ElicitationAction::Accept));
        let content = result.content.unwrap();
        assert_eq!(content["name"], serde_json::json!("Alice"));
        assert_eq!(content["age"], serde_json::json!(30));
        assert_eq!(content["active"], serde_json::json!(true));
        assert_eq!(content["score"], serde_json::json!(9.5));
        assert_eq!(content["tags"], serde_json::json!(["a", "b"]));
        assert_eq!(content["empty"], serde_json::Value::Null);
    }

    #[test]
    fn transport_elicitation_sender_form_decline() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "action": "decline"
            })
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = ElicitationRequest {
            message: "Confirm?".to_string(),
            mode: ElicitationMode::Form,
            schema: Some(serde_json::json!({"type": "object"})),
            url: None,
            elicitation_id: None,
        };

        let future = ElicitationSender::elicit(&elicitation, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert!(matches!(result.action, ElicitationAction::Decline));
        assert!(result.content.is_none());
    }

    #[test]
    fn transport_elicitation_sender_url_mode() {
        let sender = make_sender_with_responder(|req| {
            let params: serde_json::Value =
                serde_json::from_value(req.params.clone().unwrap()).unwrap();
            assert_eq!(params["mode"], "url");
            assert_eq!(params["url"], "https://example.com/auth");
            serde_json::json!({
                "action": "cancel"
            })
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = ElicitationRequest {
            message: "Please authenticate".to_string(),
            mode: ElicitationMode::Url,
            schema: None,
            url: Some("https://example.com/auth".to_string()),
            elicitation_id: Some("eid-123".to_string()),
        };

        let future = ElicitationSender::elicit(&elicitation, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert!(matches!(result.action, ElicitationAction::Cancel));
    }

    // ── TransportRootsProvider — list_roots ──────────────────────────

    #[test]
    fn transport_roots_provider_list_roots() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "roots": [
                    {"uri": "file:///home/user/project", "name": "Project"},
                    {"uri": "file:///tmp"}
                ]
            })
        });
        let roots = TransportRootsProvider::new(sender, McpContext::new(Cx::for_testing(), 0));
        let result = block_on(roots.list_roots()).unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].uri, "file:///home/user/project");
        assert_eq!(result[0].name, Some("Project".to_string()));
        assert_eq!(result[1].uri, "file:///tmp");
        assert!(result[1].name.is_none());
    }

    #[test]
    fn transport_roots_provider_maps_wire_roots_to_core_roots() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "roots": [{"uri": "file:///workspace", "name": "workspace"}]
            })
        });
        let roots = TransportRootsProvider::new(sender, McpContext::new(Cx::for_testing(), 0));

        let result = fastmcp_core::block_on(fastmcp_core::RootsProvider::list_roots(&roots))
            .expect("transport roots map into the core context type");
        assert_eq!(
            result,
            vec![ClientRoot::with_name("file:///workspace", "workspace")]
        );
    }

    #[test]
    fn transport_roots_provider_empty_roots() {
        let sender = make_sender_with_responder(|_| serde_json::json!({ "roots": [] }));
        let roots = TransportRootsProvider::new(sender, McpContext::new(Cx::for_testing(), 0));
        let result = block_on(roots.list_roots()).unwrap();
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn transport_roots_provider_trait_preserves_originating_deadline() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let sent = Arc::new(AtomicBool::new(false));
        let sent_for_transport = Arc::clone(&sent);
        let sender = RequestSender::new(
            Arc::new(PendingRequests::new()),
            Arc::new(move |_| {
                sent_for_transport.store(true, Ordering::Release);
                Ok(())
            }),
        );
        let roots = TransportRootsProvider::new(
            sender,
            McpContext::new(Cx::for_testing(), 0).with_budget_ceiling(
                asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
            ),
        );

        let error = block_on(fastmcp_core::RootsProvider::list_roots(&roots))
            .expect_err("an expired originating request must not issue roots/list");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert!(
            !sent.load(Ordering::Acquire),
            "the raw Cx must not relax the originating framework deadline ceiling"
        );
    }

    #[test]
    fn transport_roots_provider_trait_preserves_originating_cancellation() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let sent = Arc::new(AtomicBool::new(false));
        let sent_for_transport = Arc::clone(&sent);
        let sender = RequestSender::new(
            Arc::new(PendingRequests::new()),
            Arc::new(move |_| {
                sent_for_transport.store(true, Ordering::Release);
                Ok(())
            }),
        );
        let cancellation = McpRequestCancellation::new();
        cancellation.cancel();
        let roots = TransportRootsProvider::new(
            sender,
            McpContext::new(Cx::for_testing(), 0).with_request_cancellation(cancellation),
        );

        let error = block_on(fastmcp_core::RootsProvider::list_roots(&roots))
            .expect_err("a cancelled originating request must not issue roots/list");
        assert_eq!(error.code, McpErrorCode::RequestCancelled);
        assert!(
            !sent.load(Ordering::Acquire),
            "the raw Cx must not relax the originating framework cancellation"
        );
    }

    // ── RequestSender ID cleanup after success ───────────────────────

    // ── RequestSender — cancelled cx path ──────────────────────────

    #[test]
    fn request_sender_cancelled_cx_returns_cancelled_error() {
        let pending = Arc::new(PendingRequests::new());
        // Transport succeeds but never sends a response
        let send_fn: TransportSendFn = Arc::new(|_| Ok(()));
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);

        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result: McpResult<serde_json::Value> =
            block_on(sender.send_request(&cx, "test/cancel", serde_json::json!({})));
        let err = result.unwrap_err();
        assert_eq!(err.code, McpErrorCode::RequestCancelled);
    }

    // ── Elicitation request/response validation ─────────────────

    #[test]
    fn transport_elicitation_sender_rejects_missing_url_fields_before_send() {
        let sender = make_sender_with_responder(|_| {
            panic!("an invalid URL elicitation must not reach the transport")
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = ElicitationRequest {
            message: "Auth".to_string(),
            mode: ElicitationMode::Url,
            schema: None,
            url: None,
            elicitation_id: None,
        };

        let future = ElicitationSender::elicit(&elicitation, request);
        let error = fastmcp_core::block_on(future)
            .expect_err("missing URL fields must be a local input error");
        assert_eq!(error.code, McpErrorCode::InvalidParams);
        assert_eq!(error.message, INVALID_ELICITATION_REQUEST_ERROR);
    }

    #[test]
    fn transport_elicitation_sender_rejects_accepted_form_without_content() {
        let sender = make_sender_with_responder(|_| serde_json::json!({ "action": "accept" }));
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let request = ElicitationRequest::form(
            "Fill the form",
            serde_json::json!({
                "type": "object"
            }),
        );

        let error = fastmcp_core::block_on(ElicitationSender::elicit(&elicitation, request))
            .expect_err("accepted form mode must carry form content");
        assert_eq!(error.message, RESPONSE_PAYLOAD_ERROR);
    }

    #[test]
    fn transport_elicitation_sender_rejects_accepted_url_content() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "action": "accept",
                "content": {"credential": "must-not-be-exposed"}
            })
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let request = ElicitationRequest::url("Authenticate", "https://example.com", "eid-1");

        let error = fastmcp_core::block_on(ElicitationSender::elicit(&elicitation, request))
            .expect_err("accepted URL mode must not expose in-band content");
        assert_eq!(error.message, RESPONSE_PAYLOAD_ERROR);
        assert!(!error.message.contains("credential"));
    }

    #[test]
    fn transport_elicitation_sender_does_not_expose_non_accept_content() {
        let sender = make_sender_with_responder(|_| {
            serde_json::json!({
                "action": "decline",
                "content": {"credential": "must-not-be-exposed"}
            })
        });
        let elicitation =
            TransportElicitationSender::new(sender, McpContext::new(Cx::for_testing(), 0));
        let request = ElicitationRequest::form(
            "Fill the form",
            serde_json::json!({
                "type": "object"
            }),
        );

        let result = fastmcp_core::block_on(ElicitationSender::elicit(&elicitation, request))
            .expect("decline content is a SHOULD deviation, not accepted data");
        assert_eq!(result.action, ElicitationAction::Decline);
        assert!(result.content.is_none());
    }

    // ── TransportRootsProvider — transport failure ───────────────

    #[test]
    fn transport_roots_provider_transport_failure() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Err("network error".to_string()));
        let sender = RequestSender::new(pending, send_fn);
        let roots = TransportRootsProvider::new(sender, McpContext::new(Cx::for_testing(), 0));
        let result = block_on(roots.list_roots());
        assert_eq!(result.unwrap_err().message, TRANSPORT_SEND_ERROR);
    }

    #[test]
    fn transport_roots_provider_core_trait_preserves_transport_failure() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Err("network error".to_string()));
        let roots = TransportRootsProvider::new(
            RequestSender::new(pending, send_fn),
            McpContext::new(Cx::for_testing(), 0),
        );

        let error = fastmcp_core::block_on(fastmcp_core::RootsProvider::list_roots(&roots))
            .expect_err("the same transport failure must cross the core provider seam");
        assert_eq!(error.message, TRANSPORT_SEND_ERROR);
    }

    // ── SamplingSender — transport failure ───────────────────────

    #[test]
    fn transport_sampling_sender_transport_failure() {
        let pending = Arc::new(PendingRequests::new());
        let send_fn: TransportSendFn = Arc::new(|_| Err("connection reset".to_string()));
        let sender = RequestSender::new(pending, send_fn);
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![fastmcp_core::SamplingRequestMessage {
                role: SamplingRole::User,
                text: "Hi".to_string(),
            }],
            max_tokens: 10,
            system_prompt: None,
            temperature: None,
            stop_sequences: vec![],
            model_hints: vec![],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future);
        assert_eq!(result.unwrap_err().message, TRANSPORT_SEND_ERROR);
    }

    // ── SamplingSender — multiple messages ───────────────────────

    #[test]
    fn transport_sampling_sender_multiple_messages() {
        let sender = make_sender_with_responder(|req| {
            let params: serde_json::Value =
                serde_json::from_value(req.params.clone().unwrap()).unwrap();
            let messages = params["messages"].as_array().unwrap();
            assert_eq!(messages.len(), 3);
            assert_eq!(messages[0]["role"], "user");
            assert_eq!(messages[1]["role"], "assistant");
            assert_eq!(messages[2]["role"], "user");
            serde_json::json!({
                "content": {"type": "text", "text": "done"},
                "role": "assistant",
                "model": "m",
                "stopReason": "endTurn"
            })
        });
        let sampling = TransportSamplingSender::new(sender, McpContext::new(Cx::for_testing(), 0));

        let request = SamplingRequest {
            messages: vec![
                fastmcp_core::SamplingRequestMessage {
                    role: SamplingRole::User,
                    text: "Hello".to_string(),
                },
                fastmcp_core::SamplingRequestMessage {
                    role: SamplingRole::Assistant,
                    text: "Hi".to_string(),
                },
                fastmcp_core::SamplingRequestMessage {
                    role: SamplingRole::User,
                    text: "Follow up".to_string(),
                },
            ],
            max_tokens: 100,
            system_prompt: None,
            temperature: None,
            stop_sequences: vec![],
            model_hints: vec![],
        };

        let future = SamplingSender::create_message(&sampling, request);
        let result = fastmcp_core::block_on(future).unwrap();
        assert_eq!(result.text, "done");
    }

    // ── RequestSender — ID cleanup after success ────────────────

    #[test]
    fn request_sender_id_cleaned_from_pending_after_success() {
        let pending = Arc::new(PendingRequests::new());
        let pending_clone = Arc::clone(&pending);
        let send_fn: TransportSendFn = Arc::new(move |msg| {
            if let JsonRpcMessage::Request(req) = msg {
                let id = req.id.clone().unwrap();
                let response = JsonRpcResponse::success(id, serde_json::json!(null));
                pending_clone.route_response(&response);
            }
            Ok(())
        });
        let sender = RequestSender::new(Arc::clone(&pending), send_fn);
        let cx = Cx::for_testing();
        let _: serde_json::Value =
            block_on(sender.send_request(&cx, "test/method", serde_json::json!({}))).unwrap();

        // The pending request should have been consumed by route_response
        let first_id = RequestId::Number(1_000_000);
        let response = JsonRpcResponse::success(first_id, serde_json::json!(null));
        assert!(!pending.route_response(&response));
    }
}
