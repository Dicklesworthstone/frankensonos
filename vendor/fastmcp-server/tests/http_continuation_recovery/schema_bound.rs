//! Schema-bound recovery through actual OAuth/TLS and the native MRTR journal.
//! The parent peer produces every MCP response; loss is injected only after
//! native dispatch/journaling. A stricter client output contract is the single
//! changed dimension in the invalid-output cases. No response is fabricated.

use super::*;
use fastmcp_client::http_auth::tool::{ManagedToolClient, ManagedToolError};
use fastmcp_client::http_auth::tool::interaction::ManagedToolInteractionError;
use fastmcp_client::http_auth::tool::interaction::recovery::{
    ManagedToolRecoveryError, RecoverableManagedToolContinuation,
};
use fastmcp_protocol::FinalTool;

#[derive(Clone, Copy)]
pub(super) enum Case {
    Terminal,
    InvalidOutput,
    Successor,
    SuccessorInvalidOutput,
    Invalidate,
    Cancel,
    InvalidatePending,
    CancelPending,
    Abandon,
    UnreadHandback,
    NoJournal,
    SuccessorInvalidate,
}

impl Case {
    fn partial(self) -> bool {
        matches!(self, Self::Successor | Self::SuccessorInvalidOutput | Self::SuccessorInvalidate)
    }
    fn invalid_output(self) -> bool { matches!(self, Self::InvalidOutput | Self::SuccessorInvalidOutput) }
    fn stopped(self) -> bool { matches!(self, Self::Invalidate | Self::Cancel | Self::InvalidatePending | Self::CancelPending) }
}

fn definition(invalid_output: bool) -> FinalTool {
    FinalTool {
        name: "checkout".to_owned(), title: None, description: None, icons: None,
        input_schema: json!({"type":"object","properties":{"quantity":{"type":"integer"}},"required":["quantity"]}),
        output_schema: Some(json!({"type":"object","properties":{
            "quantity":{"type":if invalid_output { "string" } else { "integer" }},
            "effect":{"type":"integer","const":1}
        },"required":["quantity","effect"]})),
        annotations: None, meta: None,
    }
}

struct WakeCount(AtomicUsize);
impl std::task::Wake for WakeCount {
    fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
}

async fn lost_reply(peer: &Peer, cx: &Cx, pending: &mut RecoverableManagedToolContinuation) -> Value {
    let (reply, ()) = pair(peer.dispatch(cx, Delivery::LoseHead), async {
        let error = match pending.send(cx, RequestId::Number(2)).await {
            Err(error) => error,
            Ok(()) => pending.next_event(cx).await.err().expect("the native reply was lost"),
        };
        assert!(matches!(error, ManagedToolRecoveryError::Recovery(ContinuationRecoveryError::Interrupted)), "{error}");
        assert!(pending.is_recovery_pending());
    }).await;
    peer.quiet();
    reply
}

pub(super) async fn scenario(cx: Cx, case: Case) {
    let peer = Peer::new(&cx, !matches!(case, Case::NoJournal)).await;
    let ((), session) = pair(peer.login(), ManagedOAuthSession::authorize(
        &cx, peer.client(), OAuthSessionPolicy::default(), browser,
    )).await;
    let session = session.unwrap();
    let client = ManagedToolClient::new(session.clone(), definition(case.invalid_output())).unwrap();
    let cancellation = McpRequestCancellation::new();
    let limits = ManagedInteractionLimits::new(
        ManagedCoreLimits::new(4096, 4096, 65536, 0, Duration::from_secs(15)).unwrap(), 2, 2,
    ).unwrap();
    let (initial, operation) = pair(peer.dispatch(&cx, Delivery::Complete),
        client.start_interaction_with_cancellation(&cx, &cancellation, original(), RequestId::Number(1), limits),
    ).await;
    let mut operation = operation.unwrap();
    assert!(matches!(operation.next_event(&cx).await.unwrap(), Some(ManagedInteractionEvent::InputRequired(_))));
    assert_ne!(initial["result"]["requestState"], "handler-private-state");
    let host_effects = AtomicUsize::new(0);
    let keys: &[&str] = if case.partial() { &["left"] } else { &["left", "right"] };
    let replay = ContinuationReplayContract::for_configured_endpoint(peer.resource(), 1).unwrap();
    let mut pending = operation.prepare_recoverable_continuation(
        &cx, Some(answers(keys, &host_effects)), replay,
    ).unwrap();
    // Local ID rejection must not consume the prepared continuation or a POST.
    assert!(matches!(pending.send(&cx, RequestId::Number(1)).await,
        Err(ManagedToolRecoveryError::Recovery(ContinuationRecoveryError::Interaction(ManagedInteractionError::RepeatedRequestId)))));
    assert_eq!(pending.attempts(), 0);
    peer.quiet();

    let lost = if matches!(case, Case::InvalidatePending | Case::CancelPending | Case::Abandon) {
        pair(peer.dispatch(&cx, Delivery::StallBody), async {
            pending.send(&cx, RequestId::Number(2)).await.unwrap();
            let mut read = Box::pin(pending.next_event(&cx));
            let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
            let waker = std::task::Waker::from(Arc::clone(&wakes));
            {
                let mut task = std::task::Context::from_waker(&waker);
                assert!(read.as_mut().poll(&mut task).is_pending());
            }
            if matches!(case, Case::Abandon) {
                drop(read);
                assert!(pending.is_recovery_pending());
            } else {
                let before = wakes.0.load(Ordering::SeqCst);
                if matches!(case, Case::InvalidatePending) { client.invalidate(); }
                else { cancellation.cancel(); }
                assert!(wakes.0.load(Ordering::SeqCst) > before, "an idle socket read must be woken");
                let error = read.await.err().unwrap();
                if matches!(case, Case::InvalidatePending) {
                    assert!(matches!(error, ManagedToolRecoveryError::Tool(ManagedToolError::Invalidated)));
                } else {
                    assert!(matches!(error, ManagedToolRecoveryError::Tool(ManagedToolError::Core(ManagedCoreError::Cancelled))));
                    assert!(!client.is_invalidated());
                }
                assert!(!pending.is_recovery_pending());
            }
        }).await.0
    } else { lost_reply(&peer, &cx, &mut pending).await };
    assert!(lost.get("error").is_none(), "native dispatch must settle before injected loss: {lost}");
    assert_eq!(pending.attempts(), 1);
    assert_eq!(peer.probe.effects.load(Ordering::SeqCst), usize::from(!case.partial()));

    if matches!(case, Case::Invalidate | Case::Cancel) {
        if matches!(case, Case::Invalidate) { client.invalidate(); } else { cancellation.cancel(); }
        let error = pending.recover(&cx, RequestId::Number(3)).await.err().unwrap();
        if matches!(case, Case::Invalidate) {
            assert!(matches!(error, ManagedToolRecoveryError::Tool(ManagedToolError::Invalidated)));
        } else {
            assert!(matches!(error, ManagedToolRecoveryError::Tool(ManagedToolError::Core(ManagedCoreError::Cancelled))));
            assert!(!client.is_invalidated());
        }
        assert!(!pending.is_recovery_pending());
    }
    if !case.stopped() {
        // The failed attempt's ID stays spent, including after a dropped read.
        assert!(matches!(pending.recover(&cx, RequestId::Number(2)).await,
            Err(ManagedToolRecoveryError::Recovery(ContinuationRecoveryError::Interaction(ManagedInteractionError::RepeatedRequestId)))));
        assert_eq!(pending.attempts(), 1);
        peer.quiet();
        let (replayed, sent) = pair(peer.dispatch(&cx, Delivery::Complete),
            pending.recover(&cx, RequestId::Number(3)),
        ).await;
        sent.unwrap();
        if matches!(case, Case::UnreadHandback) {
            assert!(matches!(pending.into_interaction(&cx),
                Err(ManagedToolRecoveryError::Recovery(ContinuationRecoveryError::WrongPhase))));
        } else {
            let event = pending.next_event(&cx).await;
            if matches!(case, Case::NoJournal) {
                assert!(replayed.get("error").is_some());
                assert!(matches!(event, Err(ManagedToolRecoveryError::Recovery(_))));
                assert!(!pending.is_recovery_pending());
                assert!(pending.into_interaction(&cx).is_err());
            } else {
                assert_eq!(replayed["result"], lost["result"], "recover only the captured native reply");
                if matches!(case, Case::InvalidOutput) {
                    assert!(matches!(event, Err(ManagedToolRecoveryError::Tool(ManagedToolError::InvalidStructuredOutput))));
                    assert!(!pending.is_recovery_pending());
                    assert!(matches!(pending.recover(&cx, RequestId::Number(4)).await,
                        Err(ManagedToolRecoveryError::Tool(ManagedToolError::Closed))));
                    assert!(matches!(pending.into_interaction(&cx), Err(ManagedToolRecoveryError::Tool(ManagedToolError::Closed))));
                } else if case.partial() {
                    let ManagedInteractionEvent::InputRequired(input) = event.unwrap() else { panic!("native successor expected"); };
                    assert_eq!(input.request_state(), lost["result"]["requestState"].as_str());
                    assert_eq!(input.input_requests().unwrap().members().len(), 1);
                    assert!(input.input_requests().unwrap().get("right").is_some());
                    let mut operation = pending.into_interaction(&cx).unwrap();
                    assert_eq!(operation.pending_input().unwrap().request_state(), input.request_state());
                    let responses = answers(&["right"], &host_effects);
                    if matches!(case, Case::SuccessorInvalidate) {
                        client.invalidate();
                        assert!(operation.pending_input().is_none());
                        assert!(matches!(operation.resume(&cx, RequestId::Number(4), Some(responses)).await,
                            Err(ManagedToolInteractionError::Tool(ManagedToolError::Invalidated))));
                    } else {
                        let (final_wire, outcome) = pair(peer.dispatch(&cx, Delivery::Complete), async {
                            operation.resume(&cx, RequestId::Number(4), Some(responses)).await.unwrap();
                            operation.next_event(&cx).await
                        }).await;
                        assert_eq!(final_wire["result"]["structuredContent"]["order"], json!(["left", "right"]));
                        if case.invalid_output() {
                            assert!(matches!(outcome, Err(ManagedToolInteractionError::Tool(ManagedToolError::InvalidStructuredOutput))));
                        } else {
                            let Some(ManagedInteractionEvent::Complete(result)) = outcome.unwrap() else { panic!("native terminal expected"); };
                            assert_eq!(serde_json::from_str::<Value>(&result.encode().unwrap()).unwrap(), final_wire["result"]);
                            assert!(operation.next_event(&cx).await.unwrap().is_none());
                        }
                    }
                } else {
                    let ManagedInteractionEvent::Complete(result) = event.unwrap() else { panic!("native terminal expected"); };
                    assert_eq!(serde_json::from_str::<Value>(&result.encode().unwrap()).unwrap(), lost["result"]);
                    assert!(matches!(pending.next_event(&cx).await,
                        Err(ManagedToolRecoveryError::Recovery(ContinuationRecoveryError::WrongPhase))));
                    let mut operation = pending.into_interaction(&cx).unwrap();
                    assert!(operation.next_event(&cx).await.unwrap().is_none(), "a recovered terminal is already delivered");
                }
            }
        }
    }

    let expected_posts = if case.stopped() { 2 }
        else if matches!(case, Case::Successor | Case::SuccessorInvalidOutput) { 4 } else { 3 };
    {
        let requests = peer.seen.lock().unwrap();
        assert_eq!(requests.len(), expected_posts, "refusals must not open extra POSTs");
        if requests.len() >= 3 {
            assert_eq!(requests[1]["params"], requests[2]["params"]);
            assert_ne!(requests[1]["id"], requests[2]["id"]);
        }
        for (index, request) in requests.iter().enumerate() {
            let id: RequestId = serde_json::from_value(request["id"].clone()).unwrap();
            for previous in &requests[..index] {
                let previous: RequestId = serde_json::from_value(previous["id"].clone()).unwrap();
                assert!(!id.correlates_with(&previous));
            }
            assert_eq!(request["params"]["arguments"], json!({"quantity":1}));
        }
    }
    assert_eq!(peer.probe.starts.load(Ordering::SeqCst), 1);
    let effects = usize::from(!matches!(case, Case::SuccessorInvalidate));
    assert_eq!(peer.probe.effects.load(Ordering::SeqCst), effects);
    assert_eq!(peer.probe.transforms.load(Ordering::SeqCst), effects);
    assert_eq!(host_effects.load(Ordering::SeqCst), 2, "answers are computed once, not per transmission");
    assert_eq!(peer.grants.load(Ordering::SeqCst), 1);
    assert!(cx.checkpoint().is_ok());
    peer.quiet();

    // Local contract failure/cancellation must not close the shared login. A
    // fresh explicit sibling request uses that same credential, without refresh.
    let params = json!({"_meta":{"io.modelcontextprotocol/protocolVersion":FINAL_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities":{}}});
    let list = CoreRequest::decode(ProtocolEra::Modern2026, "tools/list", Some(&params)).unwrap();
    let (wire, ()) = pair(peer.dispatch(&cx, Delivery::Complete), async {
        let mut call = session.request_core(&cx, list, RequestId::Number(900), ManagedCoreLimits::default()).await.unwrap();
        assert!(matches!(call.next_event(&cx).await.unwrap(), Some(fastmcp_client::http_auth::rpc::ManagedCoreEvent::Result(_))));
    }).await;
    assert!(wire.get("error").is_none());
    assert_eq!(peer.seen.lock().unwrap().len(), expected_posts + 1);
    assert_eq!(peer.grants.load(Ordering::SeqCst), 1);
    peer.quiet();
    session.close();
    peer.journal.close().unwrap();
}

#[test]
fn recovered_tool_terminal_keeps_its_schema_and_effect_count() {
    isolated("schema_bound::recovered_tool_terminal_keeps_its_schema_and_effect_count", super::Case::SchemaBound(Case::Terminal));
}
#[test]
fn recovered_tool_invalid_output_cannot_escape_through_handback() {
    isolated("schema_bound::recovered_tool_invalid_output_cannot_escape_through_handback", super::Case::SchemaBound(Case::InvalidOutput));
}
#[test]
fn recovered_tool_successor_resumes_only_the_unanswered_input() {
    isolated("schema_bound::recovered_tool_successor_resumes_only_the_unanswered_input", super::Case::SchemaBound(Case::Successor));
}
#[test]
fn recovered_tool_successor_retains_final_output_validation() {
    isolated("schema_bound::recovered_tool_successor_retains_final_output_validation", super::Case::SchemaBound(Case::SuccessorInvalidOutput));
}
#[test]
fn invalidated_tool_never_posts_a_recovery() {
    isolated("schema_bound::invalidated_tool_never_posts_a_recovery", super::Case::SchemaBound(Case::Invalidate));
}
#[test]
fn cancelled_tool_recovery_preserves_the_shared_login() {
    isolated("schema_bound::cancelled_tool_recovery_preserves_the_shared_login", super::Case::SchemaBound(Case::Cancel));
}
#[test]
fn invalidation_wakes_an_idle_recovery_read_and_closes_its_socket() {
    isolated("schema_bound::invalidation_wakes_an_idle_recovery_read_and_closes_its_socket", super::Case::SchemaBound(Case::InvalidatePending));
}
#[test]
fn cancellation_wakes_an_idle_recovery_read_and_closes_its_socket() {
    isolated("schema_bound::cancellation_wakes_an_idle_recovery_read_and_closes_its_socket", super::Case::SchemaBound(Case::CancelPending));
}
#[test]
fn abandoned_tool_reply_recovers_only_through_the_native_journal() {
    isolated("schema_bound::abandoned_tool_reply_recovers_only_through_the_native_journal", super::Case::SchemaBound(Case::Abandon));
}
#[test]
fn unread_recovered_reply_cannot_become_an_unchecked_interaction() {
    isolated("schema_bound::unread_recovered_reply_cannot_become_an_unchecked_interaction", super::Case::SchemaBound(Case::UnreadHandback));
}
#[test]
fn tool_recovery_without_a_server_journal_does_not_repeat_the_effect() {
    isolated("schema_bound::tool_recovery_without_a_server_journal_does_not_repeat_the_effect", super::Case::SchemaBound(Case::NoJournal));
}
#[test]
fn recovered_successor_keeps_the_original_invalidation_domain() {
    isolated("schema_bound::recovered_successor_keeps_the_original_invalidation_domain", super::Case::SchemaBound(Case::SuccessorInvalidate));
}
