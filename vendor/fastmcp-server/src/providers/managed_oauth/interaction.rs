//! Opt-in host resolution of authenticated upstream input-required operations.
//!
//! This resolves inputs at the gateway, not by relaying upstream requestState
//! or service-account authority to downstream clients. Registration delegates
//! the configured login's authority. The host must authorize every disclosure
//! and external action using the downstream request context. No model, browser,
//! filesystem-root provider, or permissive answer is installed automatically.
//!
//! The existing managed interaction owner enforces correlated typed answers,
//! immutable arguments, fresh continuation IDs, cumulative response/input
//! limits, and one original deadline. Errors are never retry signals. This is
//! modern core execution, not Tasks, legacy, or transparent downstream MRTR.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use asupersync::Cx;
use fastmcp_client::http_auth::managed::ManagedOAuthSession;
use fastmcp_client::http_auth::rpc::ManagedCoreLimits;
use fastmcp_client::http_auth::rpc::interaction::{
    ManagedInputReply, ManagedInteractionError, ManagedInteractionLimits,
};
use fastmcp_core::{McpContext, McpError, McpErrorCode, McpResult};
use fastmcp_protocol::{
    CoreRequest, CoreResult, FinalCoreRequest, FinalCoreResult, FinalInputResponses,
    InputRequiredResult, RequestId,
};
use serde_json::{Map, Value, json};

use super::{
    BoxFuture, CoreBackend, FINAL_CLIENT_CAPABILITIES_META_KEY, Forwarder,
    ManagedOAuthProvider, NativeBackend, ProtocolEra, UNEXPECTED_RESULT, UPSTREAM_FAILURE,
    allocate_request_id, check_cx, forward_notification, upstream_error,
};

/// Local capability declarations for the explicitly supplied input handler.
/// All flags default to false. These are not copied from downstream metadata.
/// Sampling tools/context also enable basic sampling; neither enables the other.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ManagedOAuthInputCapabilities {
    pub roots: bool,
    pub sampling: bool,
    pub sampling_tools: bool,
    pub sampling_context: bool,
    pub form_elicitation: bool,
    pub url_elicitation: bool,
}

impl ManagedOAuthInputCapabilities {
    fn metadata(self) -> Value {
        let mut capabilities = Map::new();
        if self.roots {
            capabilities.insert("roots".to_owned(), json!({}));
        }
        if self.sampling || self.sampling_tools || self.sampling_context {
            let mut sampling = Map::new();
            if self.sampling_tools {
                sampling.insert("tools".to_owned(), json!({}));
            }
            if self.sampling_context {
                sampling.insert("context".to_owned(), json!({}));
            }
            capabilities.insert("sampling".to_owned(), Value::Object(sampling));
        }
        if self.form_elicitation || self.url_elicitation {
            let mut elicitation = Map::new();
            if self.form_elicitation {
                elicitation.insert("form".to_owned(), json!({}));
            }
            if self.url_elicitation {
                elicitation.insert("url".to_owned(), json!({}));
            }
            capabilities.insert("elicitation".to_owned(), Value::Object(elicitation));
        }
        Value::Object(capabilities)
    }
}

/// Which correlated answers a host callback may submit in one continuation.
/// This selects local reply handling, not an additional upstream capability.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ManagedOAuthInputResponseMode {
    /// Require every input in the current challenge. This is the default.
    #[default]
    Complete,
    /// Permit a nonempty subset when the upstream supplied nonempty continuation
    /// state. The server, not the gateway, retains already accepted answers and
    /// determines the successor challenge. No omitted answer is synthesized.
    Partial,
}

/// Limits apply across the entire forwarded operation, including host pauses.
/// Request/frame/response/time limits come from the provider's `with_limits`;
/// this policy never replaces them with a fresh budget for each continuation.
/// Interactive-login and client-credentials providers share this policy.
#[derive(Clone, Copy, Debug)]
pub struct ManagedOAuthInputPolicy {
    capabilities: ManagedOAuthInputCapabilities,
    maximum_continuations: usize,
    maximum_input_responses: usize,
    response_mode: ManagedOAuthInputResponseMode,
}

impl Default for ManagedOAuthInputPolicy {
    fn default() -> Self {
        Self {
            capabilities: ManagedOAuthInputCapabilities::default(),
            maximum_continuations: 8,
            maximum_input_responses: 256,
            response_mode: ManagedOAuthInputResponseMode::Complete,
        }
    }
}

impl ManagedOAuthInputPolicy {
    /// Validate against the managed client's hard ceilings before installation.
    /// Zero continuations requires first-response completion. Zero input answers
    /// still permits explicitly approved state-only rounds within the round cap.
    pub fn new(
        capabilities: ManagedOAuthInputCapabilities,
        maximum_continuations: usize,
        maximum_input_responses: usize,
    ) -> McpResult<Self> {
        let policy = Self {
            capabilities,
            maximum_continuations,
            maximum_input_responses,
            response_mode: ManagedOAuthInputResponseMode::Complete,
        };
        policy.limits(ManagedCoreLimits::default())?;
        Ok(policy)
    }

    /// Opt into incremental approval without changing capabilities or limits.
    /// In Partial mode a host may still return all requested answers. A proper
    /// subset requires nonempty server-owned requestState; absent and empty
    /// input maps retain their exact presence rules in both modes.
    ///
    /// Every subset consumes one continuation. Limits cover the entire operation,
    /// not a fresh allowance per subset. The complete challenge is admitted
    /// against the remaining input budget before invoking the host. Previously
    /// accepted answers are never merged into a later reply by this provider.
    #[must_use]
    pub const fn with_response_mode(mut self, mode: ManagedOAuthInputResponseMode) -> Self {
        self.response_mode = mode;
        self
    }

    /// The local reply policy retained by this provider configuration.
    #[must_use]
    pub const fn response_mode(self) -> ManagedOAuthInputResponseMode {
        self.response_mode
    }

    // Share method selection and locally declared capabilities across the two
    // authentication transports without exposing policy fields or peer overrides.
    pub(super) fn select_request(self, request: &CoreRequest) -> McpResult<Option<CoreRequest>> {
        interaction_request(request, self.capabilities)
    }

    pub(super) fn limits(self, calls: ManagedCoreLimits) -> McpResult<ManagedInteractionLimits> {
        ManagedInteractionLimits::new(
            calls, self.maximum_continuations, self.maximum_input_responses,
        ).map_err(|_| McpError::invalid_params("Invalid managed OAuth input policy"))
    }
}

/// Explicit consent/disclosure boundary for one admitted upstream challenge.
///
/// By default return all correlated typed answers. Explicitly selecting
/// [`ManagedOAuthInputResponseMode::Partial`] also permits a nonempty subset
/// when the server supplied nonempty continuation state. The next callback sees
/// only the server's successor challenge; do not repeat previously accepted
/// answers. This is not permission to perform an omitted input's side effects.
/// `None` is valid only when inputRequests was absent; a present empty map needs
/// a present empty response map in either mode. Returning an error declines the
/// operation without a continuation POST. The handler may inspect downstream
/// authentication in `ctx`, but must not equate it with the upstream service
/// account. Neither requestState nor routing can be replaced.
///
/// The future runs on the supplied request-owned `cx`, with the interaction's
/// original deadline and cancellation domain. It must cooperate with cancellation
/// and be safe to drop. Synchronous callback construction must not block. OAuth
/// session closure prevents subsequent network work; caller/request cancellation
/// or the deadline, not session closure alone, bounds an active host callback.
/// No successful host side effect can be undone if its continuation POST fails.
pub trait ManagedOAuthInputHandler: Send + Sync {
    fn resolve<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        input: Box<InputRequiredResult>,
    ) -> BoxFuture<'a, McpResult<Option<FinalInputResponses>>>;
}

impl ManagedOAuthProvider {
    /// Enable host-resolved input-required workflows for subsequently collected
    /// tools, concrete/template resources, and prompts. Existing provider clones
    /// and registered handlers keep their previous policy. Catalog and completion
    /// calls retain their original single-POST behavior and empty capabilities.
    ///
    /// Only `policy` supplies advertised upstream capabilities; downstream
    /// credentials and capability metadata are never merged into them. The shared
    /// request-ID allocator also covers every continuation, so concurrent handlers
    /// cannot reuse another handler's IDs. Calling `with_limits` afterwards retains
    /// this resolver while changing the new provider's call/catalog bounds.
    pub fn with_input_handler(
        mut self,
        policy: ManagedOAuthInputPolicy,
        handler: Arc<dyn ManagedOAuthInputHandler>,
    ) -> Self {
        self.forwarder = Arc::new(Forwarder {
            backend: Arc::new(InteractiveBackend {
                session: self.session.clone(),
                policy,
                handler,
                next_id: Arc::clone(&self.forwarder.next_id),
            }),
            next_id: Arc::clone(&self.forwarder.next_id),
            limits: self.forwarder.limits,
        });
        self
    }
}

struct InteractiveBackend {
    session: ManagedOAuthSession,
    policy: ManagedOAuthInputPolicy,
    handler: Arc<dyn ManagedOAuthInputHandler>,
    next_id: Arc<AtomicU64>,
}

impl CoreBackend for InteractiveBackend {
    fn execute<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, request: CoreRequest,
        id: RequestId, limits: ManagedCoreLimits,
    ) -> BoxFuture<'a, McpResult<FinalCoreResult>> {
        Box::pin(async move {
            let Some(interactive) = self.policy.select_request(&request)? else {
                // completion/complete is not an MRTR method. Configuring a
                // resolver must not disable the provider's completion handler.
                return NativeBackend(self.session.clone()).execute(ctx, cx, request, id, limits).await;
            };
            ctx.checkpoint()?;
            check_cx(cx)?;
            let cancellation = ctx.request_cancellation();
            let operation = self.session.start_core_interaction_with_cancellation(
                cx, &cancellation, interactive, id, self.policy.limits(limits)?,
            ).await.map_err(interaction_error)?;
            // Both paths consume the same operation owner. In particular, a
            // partial reply must not reopen an interaction with renewed budgets
            // or make a lost intermediate response eligible for automatic retry.
            let result = match self.policy.response_mode {
                ManagedOAuthInputResponseMode::Complete => operation.drive(
                    cx,
                    |input| resolve_reply(self.handler.as_ref(), ctx, cx, &self.next_id, input),
                    |notification| forward_notification(ctx, *notification)
                        .map_err(ManagedInteractionError::host_error),
                ).await,
                ManagedOAuthInputResponseMode::Partial => operation.drive_partial(
                    cx,
                    |input| resolve_reply(self.handler.as_ref(), ctx, cx, &self.next_id, input),
                    |notification| forward_notification(ctx, *notification)
                        .map_err(ManagedInteractionError::host_error),
                ).await,
            }.map_err(interaction_error)?;
            ctx.checkpoint()?;
            check_cx(cx)?;
            match *result {
                CoreResult::Final(result) => Ok(result),
                _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
            }
        })
    }
}

fn interaction_request(
    request: &CoreRequest,
    capabilities: ManagedOAuthInputCapabilities,
) -> McpResult<Option<CoreRequest>> {
    let method = match request {
        CoreRequest::Final(FinalCoreRequest::ToolsCall(_)) => "tools/call",
        CoreRequest::Final(FinalCoreRequest::ResourcesRead(_)) => "resources/read",
        CoreRequest::Final(FinalCoreRequest::PromptsGet(_)) => "prompts/get",
        _ => return Ok(None),
    };
    let mut params = request.encode_params()
        .map_err(|_| McpError::invalid_params("Invalid upstream interaction request"))?
        .ok_or_else(|| McpError::invalid_params("Missing upstream interaction parameters"))?;
    let metadata = params.get_mut("_meta").and_then(Value::as_object_mut)
        .ok_or_else(|| McpError::invalid_params("Missing upstream interaction metadata"))?;
    metadata.insert(FINAL_CLIENT_CAPABILITIES_META_KEY.to_owned(), capabilities.metadata());
    CoreRequest::decode(ProtocolEra::Modern2026, method, Some(&params))
        .map(Some)
        .map_err(|_| McpError::invalid_params("Invalid upstream interaction capabilities"))
}

async fn resolve_reply(
    handler: &dyn ManagedOAuthInputHandler,
    ctx: &McpContext,
    cx: &Cx,
    ids: &AtomicU64,
    input: Box<InputRequiredResult>,
) -> Result<ManagedInputReply, ManagedInteractionError> {
    type E = ManagedInteractionError;
    // Check before invoking even the callback's synchronous future constructor.
    E::host_checkpoint(ctx)?;
    check_cx(cx).map_err(E::host_error)?;
    let responses = handler.resolve(ctx, cx, input).await.map_err(E::host_error)?;
    E::host_checkpoint(ctx)?;
    check_cx(cx).map_err(E::host_error)?;
    Ok(ManagedInputReply {
        request_id: allocate_request_id(ids).map_err(E::host_error)?,
        input_responses: responses,
    })
}

/// How a managed-OAuth interaction error spells a host failure. It is the one
/// conversion for every interaction error type under `managed_oauth`.
///
/// Each type names only its own two variants. The cancelled variant sits at a
/// different depth in each type, so a `From` between them would change a
/// value. The mapping onto those variants is written once, in the provided
/// methods, so a new interaction type cannot copy a stale form of it.
pub(crate) trait HostDisposition: Sized {
    /// The request was cancelled.
    fn host_cancelled() -> Self;

    /// Any other host failure. It stays opaque because a host error can
    /// contain private answers or downstream identity.
    fn aborted_by_host() -> Self;

    /// Cancellation keeps its meaning; every other host error is opaque.
    fn host_error(error: McpError) -> Self {
        if error.code == McpErrorCode::RequestCancelled {
            Self::host_cancelled()
        } else {
            Self::aborted_by_host()
        }
    }

    /// `McpContext::checkpoint` fails only by cancellation, and its error is
    /// not an `McpError`, so it maps straight to the cancelled variant.
    fn host_checkpoint(ctx: &McpContext) -> Result<(), Self> {
        ctx.checkpoint().map_err(|_| Self::host_cancelled())
    }
}

impl HostDisposition for ManagedInteractionError {
    fn host_cancelled() -> Self {
        Self::Core(fastmcp_client::http_auth::rpc::ManagedCoreError::Cancelled)
    }

    fn aborted_by_host() -> Self {
        Self::AbortedByHost
    }
}

fn interaction_error(error: ManagedInteractionError) -> McpError {
    match error {
        ManagedInteractionError::Core(error) => upstream_error(error),
        ManagedInteractionError::AbortedByHost => {
            McpError::invalid_request("Authenticated upstream input was declined by the host")
        }
        _ => McpError::invalid_request(UPSTREAM_FAILURE),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use fastmcp_client::http_auth::rpc::ManagedCoreError;
    use fastmcp_core::block_on;
    use fastmcp_protocol::FinalCompletionReference;

    use super::*;
    use crate::providers::managed_oauth::core_request;

    fn roots() -> ManagedOAuthInputCapabilities {
        ManagedOAuthInputCapabilities { roots: true, ..Default::default() }
    }

    fn challenge() -> Box<InputRequiredResult> {
        let request = core_request("tools/call", json!({"name":"work"}), None).unwrap();
        let CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }) = request.decode_result(
            r#"{"resultType":"input_required","requestState":"opaque-state","inputRequests":{"roots":{"method":"roots/list"}}}"#,
        ).unwrap() else { panic!("input-required fixture") };
        Box::new(result)
    }

    #[test]
    fn capabilities_are_local_explicit_and_do_not_enable_unrelated_inputs() {
        assert_eq!(ManagedOAuthInputCapabilities::default().metadata(), json!({}));
        assert_eq!(roots().metadata(), json!({"roots":{}}));
        for (caps, expected) in [
            (ManagedOAuthInputCapabilities { sampling: true, ..Default::default() }, json!({"sampling":{}})),
            (ManagedOAuthInputCapabilities { sampling_tools: true, ..Default::default() }, json!({"sampling":{"tools":{}}})),
            (ManagedOAuthInputCapabilities { sampling_context: true, ..Default::default() }, json!({"sampling":{"context":{}}})),
            (ManagedOAuthInputCapabilities { form_elicitation: true, ..Default::default() }, json!({"elicitation":{"form":{}}})),
            (ManagedOAuthInputCapabilities { url_elicitation: true, ..Default::default() }, json!({"elicitation":{"url":{}}})),
        ] {
            assert_eq!(caps.metadata(), expected);
            assert!(caps.metadata().get("extensions").is_none());
        }
    }

    #[test]
    fn interaction_keeps_all_three_methods_arguments_and_progress_exact() {
        for (method, params) in [
            ("tools/call", json!({"name":"work", "arguments":{"_meta":{"authorization":"ordinary argument"},"x":1}})),
            ("resources/read", json!({"uri":"file:///unchanged/%2F"})),
            ("prompts/get", json!({"name":"work","arguments":{"x":"日本語"}})),
        ] {
            let original = core_request(method, params, Some(json!("own-progress"))).unwrap();
            let before = original.encode_params().unwrap();
            let selected = interaction_request(&original, roots()).unwrap().unwrap();
            let mut after = selected.encode_params().unwrap().unwrap();
            assert_eq!(after["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY], json!({"roots":{}}));
            assert_eq!(after["_meta"]["progressToken"], "own-progress");
            after["_meta"][FINAL_CLIENT_CAPABILITIES_META_KEY] = json!({});
            assert_eq!(Some(after), before);
            assert_eq!(original.encode_params().unwrap(), before);
        }
    }

    #[test]
    fn catalog_and_completion_calls_keep_the_native_single_post_path() {
        for method in ["tools/list", "resources/list", "resources/templates/list", "prompts/list"] {
            let request = core_request(method, json!({}), None).unwrap();
            assert!(interaction_request(&request, roots()).unwrap().is_none());
        }
        // Use the protocol's typed reference serialization, not a second wire schema.
        let reference: FinalCompletionReference = serde_json::from_value(json!({"type":"ref/prompt","name":"work"})).unwrap();
        let request = core_request("completion/complete", json!({
            "ref": reference, "argument":{"name":"x","value":"a"}
        }), None).unwrap();
        assert!(interaction_request(&request, roots()).unwrap().is_none());
    }

    #[test]
    fn input_policy_uses_the_managed_clients_hard_ceilings() {
        assert!(ManagedOAuthInputPolicy::new(roots(), 64, 1024).is_ok());
        assert!(ManagedOAuthInputPolicy::new(roots(), 65, 1024).is_err());
        assert!(ManagedOAuthInputPolicy::new(roots(), 64, 1025).is_err());
        assert!(ManagedOAuthInputPolicy::new(roots(), 0, 0).is_ok());
    }

    #[test]
    fn partial_input_replies_require_explicit_local_opt_in() {
        assert_eq!(ManagedOAuthInputResponseMode::default(), ManagedOAuthInputResponseMode::Complete);
        assert_eq!(ManagedOAuthInputPolicy::default().response_mode(), ManagedOAuthInputResponseMode::Complete);
        let original = ManagedOAuthInputPolicy::new(roots(), 2, 3).unwrap();
        let partial = original.with_response_mode(ManagedOAuthInputResponseMode::Partial);
        assert_eq!(original.response_mode(), ManagedOAuthInputResponseMode::Complete);
        assert_eq!(partial.response_mode(), ManagedOAuthInputResponseMode::Partial);
        assert_eq!(partial.capabilities, original.capabilities);
        assert_eq!(partial.maximum_continuations, 2);
        assert_eq!(partial.maximum_input_responses, 3);
        assert_eq!(
            partial.with_response_mode(ManagedOAuthInputResponseMode::Complete).response_mode(),
            ManagedOAuthInputResponseMode::Complete,
        );
    }

    #[derive(Clone, Copy)]
    enum Action { Answer, StateOnly, Decline, CancelRequest, CancelContext }

    struct Host { calls: AtomicUsize, action: Action }
    impl ManagedOAuthInputHandler for Host {
        fn resolve<'a>(
            &'a self, ctx: &'a McpContext, cx: &'a Cx, input: Box<InputRequiredResult>,
        ) -> BoxFuture<'a, McpResult<Option<FinalInputResponses>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                assert_eq!(input.request_state(), Some("opaque-state"));
                match self.action {
                    Action::Decline => return Err(McpError::invalid_params("PRIVATE-HOST-ERROR")),
                    Action::CancelRequest => { ctx.request_cancellation().cancel(); }
                    Action::CancelContext => { cx.set_cancel_requested(true); }
                    _ => {},
                }
                Ok(if matches!(self.action, Action::StateOnly) { None } else {
                    Some(serde_json::from_value(json!({"roots":{"roots":[]}})).unwrap())
                })
            })
        }
    }

    #[test]
    fn successful_host_answers_use_the_shared_allocator_once() {
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(10);
        let host = Host { calls: AtomicUsize::new(0), action: Action::Answer };
        let before = allocate_request_id(&ids).unwrap();
        let reply = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).unwrap();
        let after = allocate_request_id(&ids).unwrap();
        assert!(!reply.request_id.correlates_with(&before));
        assert!(!reply.request_id.correlates_with(&after));
        assert_eq!(reply.input_responses.unwrap().len(), 1);
        assert_eq!(ids.load(Ordering::SeqCst), 13);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_owners_do_not_construct_host_futures_or_allocate_ids() {
        for cancel_context in [false, true] {
            let ctx = McpContext::new(Cx::for_testing(), 1);
            let cx = Cx::for_testing();
            if cancel_context { cx.set_cancel_requested(true); } else { ctx.request_cancellation().cancel(); }
            let ids = AtomicU64::new(1);
            let host = Host { calls: AtomicUsize::new(0), action: Action::Answer };
            let error = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
            assert_eq!(interaction_error(error).code, McpErrorCode::RequestCancelled);
            assert_eq!(host.calls.load(Ordering::SeqCst), 0);
            assert_eq!(ids.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn host_cancellation_withholds_answers_and_does_not_cancel_siblings() {
        for action in [Action::CancelRequest, Action::CancelContext] {
            let ctx = McpContext::new(Cx::for_testing(), 1);
            let cx = Cx::for_testing();
            let sibling = Cx::for_testing();
            let ids = AtomicU64::new(1);
            let host = Host { calls: AtomicUsize::new(0), action };
            let error = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
            assert_eq!(interaction_error(error).code, McpErrorCode::RequestCancelled);
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            assert_eq!(ids.load(Ordering::SeqCst), 1);
            assert!(!sibling.is_cancel_requested());
        }
    }

    /// The cancellation checkpoint that follows the host callback, graded by
    /// variant name rather than by any weaker predicate.
    ///
    /// Every path out of `resolve_reply` that reaches cancellation is an
    /// `Err`, so `is_err()` cannot grade this one. Two stronger-looking
    /// predicates are also too coarse:
    ///
    /// * `AbortedByHost` is the other error this same call can produce, and
    ///   the planted negative below reaches it by changing one field.
    /// * `interaction_error(..).code` routes `Core(_)` through
    ///   `upstream_error`, which maps `Cancelled` **and** `TimedOut` onto one
    ///   `RequestCancelled` code. The separate test below proves that collapse.
    ///
    /// The three state assertions place the failure at the checkpoint after
    /// the callback rather than at another guard in the same call: the host
    /// ran exactly once, so both entry guards admitted it; no continuation ID
    /// was allocated, so the failure precedes `allocate_request_id`; and `cx`
    /// reports no cancellation request, which is what rules out the `check_cx`
    /// guard that follows.
    ///
    /// That last step is worth stating exactly, because it is the weakest one.
    /// `Cx::checkpoint` fails on cancellation *or* on budget exhaustion
    /// (deadline, poll quota, cost), and `check_cx` maps every one of those
    /// onto this same `Cancelled` variant. So the attribution holds because
    /// `Cx::for_testing()` arms no budget, not because the variant could
    /// distinguish the two guards -- it cannot.
    #[test]
    fn host_cancelled_requests_fail_the_post_callback_checkpoint_by_variant() {
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(7);
        let host = Host { calls: AtomicUsize::new(0), action: Action::CancelRequest };

        let error = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();

        assert!(
            matches!(error, ManagedInteractionError::Core(ManagedCoreError::Cancelled)),
            "the post-callback checkpoint must yield Core(Cancelled), not {error:?}",
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(!cx.is_cancel_requested());
        assert_eq!(ids.load(Ordering::SeqCst), 7);
    }

    /// Planted negative for the test above. Exactly one input differs -- the
    /// host's `action` -- and exactly one verdict flips: the named variant.
    ///
    /// A declining host stops the same call with the same observable state:
    /// the callback still ran once, `cx` is still uncancelled, and no ID was
    /// allocated. So every assertion the positive makes about state holds here
    /// unchanged, and an `is_err()` positive would accept this row as though
    /// it were the cancellation it exists to prove. Naming the variant is what
    /// refuses it.
    #[test]
    fn host_cancelled_requests_fail_the_post_callback_checkpoint_planted_negative() {
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(7);
        // Only `action` differs from the positive above.
        let host = Host { calls: AtomicUsize::new(0), action: Action::Decline };

        let error = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();

        assert!(
            matches!(error, ManagedInteractionError::AbortedByHost),
            "a declining host must be refused as AbortedByHost, not {error:?}",
        );
        assert!(!matches!(error, ManagedInteractionError::Core(ManagedCoreError::Cancelled)));
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert!(!cx.is_cancel_requested());
        assert_eq!(ids.load(Ordering::SeqCst), 7);
    }

    /// Why the pair above names the variant instead of asserting the converted
    /// code: `upstream_error` maps two distinct core errors onto one wire code,
    /// so `interaction_error(..).code == RequestCancelled` cannot separate a
    /// cancelled interaction from a timed-out one. The `AbortedByHost` row is
    /// the control -- it proves the converted code does discriminate something,
    /// so the two equal rows above it are a real collapse and not a converter
    /// that answers `RequestCancelled` to everything.
    #[test]
    fn the_converted_code_cannot_separate_cancelled_from_timed_out() {
        for variant in [ManagedCoreError::Cancelled, ManagedCoreError::TimedOut] {
            assert_eq!(
                interaction_error(ManagedInteractionError::Core(variant)).code,
                McpErrorCode::RequestCancelled,
            );
        }
        assert_eq!(
            interaction_error(ManagedInteractionError::AbortedByHost).code,
            McpErrorCode::InvalidRequest,
        );
    }

    #[test]
    fn declined_host_errors_are_redacted_and_cannot_allocate_a_continuation() {
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(1);
        let host = Host { calls: AtomicUsize::new(0), action: Action::Decline };
        let error = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
        assert!(matches!(error, ManagedInteractionError::AbortedByHost));
        assert!(!interaction_error(error).to_string().contains("PRIVATE-HOST-ERROR"));
        assert_eq!(ids.load(Ordering::SeqCst), 1);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn host_state_only_answer_stays_absent_instead_of_becoming_an_empty_map() {
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(1);
        let host = Host { calls: AtomicUsize::new(0), action: Action::StateOnly };
        let reply = block_on(resolve_reply(&host, &ctx, &cx, &ids, challenge())).unwrap();
        assert!(reply.input_responses.is_none());
        // Matching the answer to its challenge remains with the protocol driver;
        // the host adapter must neither synthesize answers nor mutate the input.
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
    }
}
