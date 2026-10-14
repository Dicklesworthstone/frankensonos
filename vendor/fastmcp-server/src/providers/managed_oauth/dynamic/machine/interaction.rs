//! Machine-credential integration with the shared host-consent boundary.
//!
//! One native interaction owner covers discovery, every continuation and host
//! pause. This adapter cannot replace its requestState, renew its budgets, or
//! replay a failed POST. Native cancellation also observes machine-owner close.

use std::sync::Arc;
use std::sync::atomic::AtomicU64;

use asupersync::Cx;
use fastmcp_client::http_auth::discovery::client_credentials::rpc::interaction::{
    ClientCredentialsInputReply, ClientCredentialsInteraction, ClientCredentialsInteractionError,
};
use fastmcp_client::http_auth::rpc::{ManagedCoreError, ManagedCoreEvent};
use fastmcp_client::http_auth::rpc::interaction::ManagedInteractionError;
use fastmcp_core::{McpContext, McpError, McpResult};
use fastmcp_protocol::InputRequiredResult;

use crate::providers::managed_oauth::interaction::{
    HostDisposition, ManagedOAuthInputHandler, ManagedOAuthInputPolicy,
    ManagedOAuthInputResponseMode,
};
use super::{
    BoxFuture, ClientCredentialsCoreError, ClientCredentialsProvider, Forwarder,
    MACHINE_FAILURE, MachineBackend, MachineResponse, check_cx, forward_notification,
    machine_error, next_pair, upstream_error,
};

const HOST_DECLINED: &str = "Machine-authenticated upstream input was declined by the host";

pub(super) struct MachineInputs {
    pub(super) policy: ManagedOAuthInputPolicy,
    pub(super) handler: Arc<dyn ManagedOAuthInputHandler>,
    pub(super) next_id: Arc<AtomicU64>,
}

impl ClientCredentialsProvider {
    /// Enable host-resolved input-required workflows for subsequently collected
    /// tools, concrete/template resources and prompts. Existing provider clones
    /// and registered handlers keep their previous behavior. Calling `with_limits`
    /// afterwards retains this input handler with the new call limits.
    ///
    /// The host explicitly supplies input capabilities and a consent/disclosure
    /// callback. Downstream capabilities and identity do not grant authority to
    /// answer an upstream input as this machine. Nothing automatically starts a
    /// model, opens a URL, reveals roots or accepts an elicitation. Callback errors
    /// decline the operation, without a continuation POST or diagnostic leakage.
    ///
    /// Complete answers are the default; the shared policy can explicitly enable
    /// partial answers. Native correlation, opaque state, original arguments and
    /// cumulative input/response/round limits are unchanged. Discovery and every
    /// continuation use distinct IDs from the same provider-wide allocator.
    ///
    /// The original native deadline, request cancellation and machine-owner close
    /// bound host pauses. Callbacks must not block during construction or polling
    /// and must be safe to drop. Cancellation cannot undo a host side effect that
    /// already happened. This is local host resolution, not transparent downstream
    /// MRTR or Tasks relay. Catalogs and completion stay on the core-only path.
    pub fn with_input_handler(
        mut self,
        policy: ManagedOAuthInputPolicy,
        handler: Arc<dyn ManagedOAuthInputHandler>,
    ) -> Self {
        let next_id = Arc::clone(&self.forwarder.next_id);
        self.forwarder = Arc::new(Forwarder {
            backend: Arc::new(MachineBackend {
                source: Arc::clone(&self.source),
                next_id: Arc::clone(&next_id),
                inputs: Some(Arc::new(MachineInputs {
                    policy, handler, next_id: Arc::clone(&next_id),
                })),
            }),
            next_id,
            limits: self.forwarder.limits,
        });
        self
    }
}

/// Consumed on its first read. Dropping that read retires the interaction and
/// any parked host callback instead of retaining reusable continuation state.
pub(super) struct InteractiveMachineResponse {
    operation: Option<ClientCredentialsInteraction>,
    context: McpContext,
    inputs: Arc<MachineInputs>,
}

impl InteractiveMachineResponse {
    pub(super) fn new(
        operation: ClientCredentialsInteraction,
        context: McpContext,
        inputs: Arc<MachineInputs>,
    ) -> Self {
        Self { operation: Some(operation), context, inputs }
    }
}

impl MachineResponse for InteractiveMachineResponse {
    fn next_event<'a>(&'a mut self, cx: &'a Cx)
        -> BoxFuture<'a, McpResult<Option<ManagedCoreEvent>>>
    {
        Box::pin(async move {
            let operation = self.operation.take()
                .ok_or_else(|| McpError::invalid_request(MACHINE_FAILURE))?;
            let ctx = &self.context;
            let inputs = &self.inputs;
            ctx.checkpoint()?;
            check_cx(cx)?;
            // Both drivers consume the exact same native owner. No round opens
            // a new interaction, reconstructs state, or replenishes its limits.
            // Native drive guards the host callback with its original deadline
            // and machine/request cancellation, even while the callback parks.
            let result = match inputs.policy.response_mode() {
                ManagedOAuthInputResponseMode::Complete => operation.drive(
                    cx,
                    |input| resolve_reply(inputs.handler.as_ref(), ctx, cx, &inputs.next_id, input),
                    |notification| forward_notification(ctx, *notification)
                        .map_err(ClientCredentialsInteractionError::host_error),
                ).await,
                ManagedOAuthInputResponseMode::Partial => operation.drive_partial(
                    cx,
                    |input| resolve_reply(inputs.handler.as_ref(), ctx, cx, &inputs.next_id, input),
                    |notification| forward_notification(ctx, *notification)
                        .map_err(ClientCredentialsInteractionError::host_error),
                ).await,
            }.map_err(interaction_error)?;
            ctx.checkpoint()?;
            check_cx(cx)?;
            // Notifications were forwarded once by drive; the outer backend
            // sees only the complete request-typed terminal result.
            Ok(Some(ManagedCoreEvent::Result(result)))
        })
    }
}

async fn resolve_reply(
    handler: &dyn ManagedOAuthInputHandler,
    ctx: &McpContext,
    cx: &Cx,
    ids: &AtomicU64,
    input: Box<InputRequiredResult>,
) -> Result<ClientCredentialsInputReply, ClientCredentialsInteractionError> {
    type E = ClientCredentialsInteractionError;
    // A cancelled request must not even construct an application callback future.
    E::host_checkpoint(ctx)?;
    check_cx(cx).map_err(E::host_error)?;
    let input_responses = handler.resolve(ctx, cx, input).await.map_err(E::host_error)?;
    E::host_checkpoint(ctx)?;
    check_cx(cx).map_err(E::host_error)?;
    let (discovery_id, request_id) = next_pair(ids).map_err(E::host_error)?;
    Ok(ClientCredentialsInputReply { discovery_id, request_id, input_responses })
}

/// This type's spelling of the shared host dispositions. The cancelled
/// variant is nested one level deeper than `ManagedInteractionError`'s, which
/// is why each type names its own variants rather than converting.
impl HostDisposition for ClientCredentialsInteractionError {
    fn host_cancelled() -> Self {
        Self::Core(ClientCredentialsCoreError::Protocol(ManagedCoreError::Cancelled))
    }

    fn aborted_by_host() -> Self {
        Self::Interaction(ManagedInteractionError::AbortedByHost)
    }
}

pub(super) fn interaction_error(error: ClientCredentialsInteractionError) -> McpError {
    match error {
        ClientCredentialsInteractionError::Core(error) => machine_error(error),
        ClientCredentialsInteractionError::Interaction(ManagedInteractionError::Core(error)) => {
            upstream_error(error)
        }
        ClientCredentialsInteractionError::Interaction(ManagedInteractionError::AbortedByHost) => {
            McpError::invalid_request(HOST_DECLINED)
        }
        ClientCredentialsInteractionError::Interaction(_) => McpError::invalid_request(MACHINE_FAILURE),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmcp_core::McpErrorCode;
    use std::future::{Future, pending};
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};
    use fastmcp_protocol::{CoreResult, FinalCoreResult, FinalInputResponses};
    use serde_json::json;
    use crate::providers::managed_oauth::core_request;

    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = pin!(future);
        match future.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(result) => result,
            Poll::Pending => panic!("unexpected fixture suspension"),
        }
    }

    fn challenge() -> Box<InputRequiredResult> {
        let request = core_request("tools/call", json!({"name":"work"}), None).unwrap();
        let CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }) = request.decode_result(
            r#"{"resultType":"input_required","requestState":"opaque-machine-state","inputRequests":{"roots":{"method":"roots/list"}}}"#,
        ).unwrap() else { panic!("input-required fixture") };
        Box::new(result)
    }

    #[derive(Clone, Copy)]
    enum Action { Answer, Absent, Empty, Decline, CancelRequest, CancelContext, Park }

    struct Host {
        calls: AtomicUsize,
        drops: Arc<AtomicUsize>,
        action: Action,
    }

    struct Dropped(Arc<AtomicUsize>);
    impl Drop for Dropped {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    impl ManagedOAuthInputHandler for Host {
        fn resolve<'a>(
            &'a self, ctx: &'a McpContext, cx: &'a Cx, input: Box<InputRequiredResult>,
        ) -> BoxFuture<'a, McpResult<Option<FinalInputResponses>>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let dropped = Dropped(Arc::clone(&self.drops));
            Box::pin(async move {
                let _owner = dropped;
                assert_eq!(ctx.request_id(), 81);
                assert_eq!(input.request_state(), Some("opaque-machine-state"));
                match self.action {
                    Action::Decline => return Err(McpError::invalid_request("PRIVATE-MACHINE-ANSWER")),
                    Action::CancelRequest => { ctx.request_cancellation().cancel(); }
                    Action::CancelContext => { cx.set_cancel_requested(true); }
                    Action::Park => pending::<()>().await,
                    _ => {},
                }
                Ok(match self.action {
                    Action::Absent => None,
                    Action::Empty => Some(serde_json::from_value(json!({})).unwrap()),
                    _ => Some(serde_json::from_value(json!({"roots":{"roots":[]}})).unwrap()),
                })
            })
        }
    }

    fn host(action: Action) -> Host {
        Host { calls: AtomicUsize::new(0), drops: Arc::new(AtomicUsize::new(0)), action }
    }

    #[test]
    fn approved_answers_allocate_two_fresh_ids_from_the_shared_domain() {
        let ctx = McpContext::new(Cx::for_testing(), 81);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(10);
        let host = host(Action::Answer);
        let (before, previous) = next_pair(&ids).unwrap();
        let reply = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).unwrap();
        let (after, following) = next_pair(&ids).unwrap();
        let all = [before, previous, reply.discovery_id, reply.request_id, after, following];
        for (index, id) in all.iter().enumerate() {
            assert!(all[..index].iter().all(|other| !id.correlates_with(other)));
        }
        assert_eq!(reply.input_responses.unwrap().len(), 1);
        assert_eq!(ids.load(Ordering::SeqCst), 16);
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn cancelled_request_or_context_never_constructs_a_host_future() {
        for cancel_context in [false, true] {
            let ctx = McpContext::new(Cx::for_testing(), 81);
            let cx = Cx::for_testing();
            if cancel_context { cx.set_cancel_requested(true); } else { ctx.request_cancellation().cancel(); }
            let ids = AtomicU64::new(1);
            let host = host(Action::Answer);
            let error = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
            assert_eq!(interaction_error(error).code, McpErrorCode::RequestCancelled);
            assert_eq!(host.calls.load(Ordering::SeqCst), 0);
            assert_eq!(host.drops.load(Ordering::SeqCst), 0);
            assert_eq!(ids.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn cancellation_during_host_work_withholds_answers_and_continuation_ids() {
        for action in [Action::CancelRequest, Action::CancelContext] {
            let ctx = McpContext::new(Cx::for_testing(), 81);
            let cx = Cx::for_testing();
            let sibling = McpContext::new(cx.clone(), 82);
            let ids = AtomicU64::new(1);
            let host = host(action);
            let error = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
            assert_eq!(interaction_error(error).code, McpErrorCode::RequestCancelled);
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            assert_eq!(host.drops.load(Ordering::SeqCst), 1);
            assert_eq!(ids.load(Ordering::SeqCst), 1);
            assert!(!sibling.request_cancellation().is_cancel_requested());
            if matches!(action, Action::CancelRequest) {
                assert!(!cx.is_cancel_requested());
            }
        }
    }

    #[test]
    fn declined_answers_do_not_leak_host_errors_or_allocate_ids() {
        let ctx = McpContext::new(Cx::for_testing(), 81);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(1);
        let host = host(Action::Decline);
        let error = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();
        let error = interaction_error(error);
        assert_eq!(error.message, HOST_DECLINED);
        assert!(!error.to_string().contains("PRIVATE-MACHINE-ANSWER"));
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(ids.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn absent_and_present_empty_host_answers_remain_distinct() {
        for (action, present) in [(Action::Absent, false), (Action::Empty, true)] {
            let ctx = McpContext::new(Cx::for_testing(), 81);
            let cx = Cx::for_testing();
            let ids = AtomicU64::new(1);
            let reply = ready(resolve_reply(&host(action), &ctx, &cx, &ids, challenge())).unwrap();
            assert_eq!(reply.input_responses.is_some(), present);
            if let Some(responses) = reply.input_responses { assert!(responses.is_empty()); }
            assert!(!reply.discovery_id.correlates_with(&reply.request_id));
            // The native driver, not this adapter, checks the answer shape
            // against its retained challenge before any continuation dispatch.
        }
    }

    #[test]
    fn abandoning_a_parked_host_drops_it_without_allocating_continuation_ids() {
        let ctx = McpContext::new(Cx::for_testing(), 81);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(1);
        let host = host(Action::Park);
        {
            let mut future = pin!(resolve_reply(&host, &ctx, &cx, &ids, challenge()));
            assert!(future.as_mut().poll(&mut Context::from_waker(Waker::noop())).is_pending());
            assert_eq!(host.calls.load(Ordering::SeqCst), 1);
            assert_eq!(host.drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(host.drops.load(Ordering::SeqCst), 1);
        assert_eq!(ids.load(Ordering::SeqCst), 1);
        assert!(!cx.is_cancel_requested());
        assert!(!ctx.request_cancellation().is_cancel_requested());
    }

    /// The checkpoint after the host callback refuses a request cancelled
    /// *during* that callback, and names this module's own error type.
    ///
    /// The three sibling tests above reach the same disposition through
    /// `interaction_error(..).code == RequestCancelled`. That code cannot
    /// separate a cancelled interaction from a timed-out one, and it cannot
    /// separate this module's `Core(Protocol(Cancelled))` from the
    /// `Interaction(Core(Cancelled))` that `managed_oauth::interaction`
    /// produces for the same input -- the two conversions land in *different
    /// outer arms* and collapse onto one wire code. Naming the variant is the
    /// only assertion that holds them apart.
    ///
    /// Attribution: the host ran exactly once and was dropped, so both entry
    /// guards admitted it and the failure follows the callback; no ID was
    /// allocated, so it precedes `next_pair`; and `cx` reports no cancellation
    /// request, which rules out the `check_cx` guard on the next line.
    /// `Cx::for_testing()` arms no budget, so `checkpoint` can fail here only
    /// on cancellation -- the attribution holds because nothing else is armed,
    /// not because the variant could distinguish the two guards.
    #[test]
    fn host_cancelled_machine_requests_fail_the_post_callback_checkpoint_by_variant() {
        let ctx = McpContext::new(Cx::for_testing(), 81);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(7);
        let host = host(Action::CancelRequest);

        let error = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();

        assert!(
            matches!(
                error,
                ClientCredentialsInteractionError::Core(ClientCredentialsCoreError::Protocol(
                    ManagedCoreError::Cancelled,
                )),
            ),
            "the post-callback checkpoint must yield Core(Protocol(Cancelled)), not {error:?}",
        );
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.drops.load(Ordering::SeqCst), 1);
        assert!(!cx.is_cancel_requested());
        assert_eq!(ids.load(Ordering::SeqCst), 7);
    }

    /// Planted negative for the test above. Exactly one input differs -- the
    /// host's `action` -- and exactly one verdict flips: the named variant.
    ///
    /// A declining host stops the same call with the same observable state:
    /// the callback still ran once and was dropped, `cx` is still uncancelled,
    /// and no ID was allocated. Every state assertion the positive makes holds
    /// here unchanged, so an `is_err()` positive would accept this row as
    /// though it were the cancellation it exists to prove.
    ///
    /// It also separates the two outer arms. A decline routes through
    /// `host_error` into `Interaction(AbortedByHost)`; only the checkpoint
    /// reaches `Core(Protocol(..))`. A test that matched on the nested
    /// `ManagedCoreError` alone, or on `interaction_error(..).code`, could not
    /// tell those two apart.
    #[test]
    fn host_cancelled_machine_requests_fail_the_post_callback_checkpoint_planted_negative() {
        let ctx = McpContext::new(Cx::for_testing(), 81);
        let cx = Cx::for_testing();
        let ids = AtomicU64::new(7);
        // Only `action` differs from the positive above.
        let host = host(Action::Decline);

        let error = ready(resolve_reply(&host, &ctx, &cx, &ids, challenge())).err().unwrap();

        assert!(
            matches!(
                error,
                ClientCredentialsInteractionError::Interaction(
                    ManagedInteractionError::AbortedByHost,
                ),
            ),
            "a declining host must be refused as Interaction(AbortedByHost), not {error:?}",
        );
        assert!(!matches!(
            error,
            ClientCredentialsInteractionError::Core(ClientCredentialsCoreError::Protocol(
                ManagedCoreError::Cancelled,
            )),
        ));
        assert_eq!(host.calls.load(Ordering::SeqCst), 1);
        assert_eq!(host.drops.load(Ordering::SeqCst), 1);
        assert!(!cx.is_cancel_requested());
        assert_eq!(ids.load(Ordering::SeqCst), 7);
    }

    /// The shared `HostDisposition::host_error` on this type: a host error
    /// carrying `RequestCancelled` keeps its meaning as this type's nested
    /// cancelled variant.
    #[test]
    fn host_disposition_maps_a_cancelled_host_error_to_the_nested_cancelled_variant() {
        let error = ClientCredentialsInteractionError::host_error(McpError::new(
            McpErrorCode::RequestCancelled,
            "host failure",
        ));

        assert!(
            matches!(
                error,
                ClientCredentialsInteractionError::Core(ClientCredentialsCoreError::Protocol(
                    ManagedCoreError::Cancelled,
                )),
            ),
            "a cancelled host error must yield Core(Protocol(Cancelled)), not {error:?}",
        );
    }

    /// Planted negative for the test above: only the host error's code
    /// differs, and the result must be the opaque abort, not cancellation.
    #[test]
    fn host_disposition_maps_any_other_host_error_to_an_opaque_abort() {
        let error = ClientCredentialsInteractionError::host_error(McpError::new(
            McpErrorCode::InvalidRequest,
            "host failure",
        ));

        assert!(
            matches!(
                error,
                ClientCredentialsInteractionError::Interaction(
                    ManagedInteractionError::AbortedByHost,
                ),
            ),
            "any other host error must yield Interaction(AbortedByHost), not {error:?}",
        );
    }
}
