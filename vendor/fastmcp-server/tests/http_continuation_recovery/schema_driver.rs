//! Schema-bound host resolution through native OAuth, TLS and one-use MRTR.
//! The existing native endpoint produces every challenge, successor and result.
//! Request headers are asserted on the real socket, then forwarded unchanged.
//! No model, browser action, external provider, or durable recovery is simulated
//! as working product capability by these local composition tests.

use super::*;
use fastmcp_client::http_auth::rpc::ManagedCoreEvent;
use fastmcp_client::http_auth::rpc::interaction::ManagedInputReply;
use fastmcp_client::http_auth::tool::{ManagedToolClient, ManagedToolError};
use fastmcp_client::http_auth::tool::interaction::ManagedToolInteractionError;
use fastmcp_protocol::FinalTool;
use std::sync::atomic::AtomicBool;

#[derive(Clone, Copy, Debug)]
pub(super) enum Case {
    Complete, NoHeaders, Partial, Observed, InvalidOutput, PartialInvalidOutput,
    Decline, WrongAnswer, RepeatedId, InvalidateConstructor, InvalidateReady,
    CancelReady, InvalidatePending, CancelPending, AbandonPending,
    PartialRoundLimit, PreInvalidated, LostReply, AlreadyComplete,
}

impl Case {
    fn partial(self) -> bool {
        matches!(self, Self::Partial | Self::PartialInvalidOutput | Self::PartialRoundLimit)
    }
    fn pending(self) -> bool {
        matches!(self, Self::InvalidatePending | Self::CancelPending | Self::AbandonPending)
    }
    fn continuations(self) -> usize {
        match self {
            Self::Partial | Self::PartialInvalidOutput => 2,
            Self::Complete | Self::NoHeaders | Self::Observed | Self::InvalidOutput
                | Self::PartialRoundLimit | Self::LostReply | Self::AlreadyComplete => 1,
            _ => 0,
        }
    }
}

fn definition(invalid_output: bool) -> FinalTool {
    FinalTool {
        name: "checkout".to_owned(), title: None, description: None, icons: None,
        input_schema: json!({"type":"object", "properties":{
            "quantity":{"type":"integer","minimum":1,"x-mcp-header":"Quantity"}
        },"required":["quantity"]}),
        output_schema: Some(json!({"type":"object", "properties":{
            "quantity":{"type":if invalid_output { "string" } else { "integer" }}
        },"required":["quantity"]})),
        annotations: None, meta: None,
    }
}

// The existing Peer owns TLS, authentication, native dispatch and injected loss.
// This new path adds header assertions without weakening the old fixture path.
impl Peer {
    async fn dispatch_driver(&self, cx: &Cx, id: i64, reviewed: bool, delivery: Delivery) -> Value {
        let (mut socket, start, headers, body) = self.receive().await;
        assert_eq!(start, "POST /mcp HTTP/1.1");
        assert_eq!(headers["authorization"], format!("Bearer {}", self.token));
        assert_eq!(headers["mcp-protocol-version"], FINAL_PROTOCOL_VERSION);
        assert_eq!(headers["mcp-method"], "tools/call");
        assert_eq!(headers["mcp-name"], "checkout");
        assert!(!headers.contains_key("mcp-session-id") && !headers.contains_key("last-event-id"));
        let fields: Vec<_> = headers.iter().filter(|(name, _)| name.starts_with("mcp-param-")).collect();
        if reviewed {
            assert_eq!(fields.len(), 1);
            assert_eq!(headers["mcp-param-quantity"], "1");
        } else {
            assert!(fields.is_empty(), "annotations must not enable disclosure without review");
        }
        let wire: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(wire["id"], id);
        assert_eq!(wire["method"], "tools/call");
        assert_eq!(wire["params"]["arguments"], json!({"quantity":1}));
        self.seen.lock().unwrap().push(wire);
        let mut request = HttpRequest::new(HttpMethod::Post, "/mcp");
        for (name, value) in headers { request = request.with_header(name, value); }
        request = request.with_body(body);
        let reply = Box::pin(self.endpoint.handle_secured_async(cx, &self.policy, request)).await.unwrap();
        assert!(!reply.is_streaming());
        let (response, stream) = reply.into_parts();
        assert!(stream.is_none());
        let value: Value = serde_json::from_slice(&response.body).unwrap();
        assert!(value.get("error").is_none(), "the native server must execute before delivery: {value}");
        write_reply(&mut socket, response.status.0, &response.headers, &response.body, delivery).await;
        value
    }
}

struct ResolutionGuard(Arc<AtomicBool>);
impl Drop for ResolutionGuard {
    fn drop(&mut self) { self.0.store(true, Ordering::SeqCst); }
}

pub(super) async fn scenario(cx: Cx, case: Case) {
    let peer = Peer::new(&cx, true).await;
    let ((), session) = pair(peer.login(), ManagedOAuthSession::authorize(
        &cx, peer.client(), OAuthSessionPolicy::default(), browser,
    )).await;
    let session = session.unwrap();
    let invalid_output = matches!(case, Case::InvalidOutput | Case::PartialInvalidOutput);
    let tool = ManagedToolClient::new(session.clone(), definition(invalid_output)).unwrap();
    let reviewed = !matches!(case, Case::NoHeaders);
    let tool = if reviewed {
        tool.review_headers(|binding| binding.header_name() == "Mcp-Param-Quantity"
            && binding.property_path() == ["quantity".to_owned()]).unwrap()
    } else { tool };
    let cancellation = McpRequestCancellation::new();
    let limits = ManagedInteractionLimits::new(
        ManagedCoreLimits::new(4096, 4096, 65536, 0, Duration::from_secs(15)).unwrap(),
        if matches!(case, Case::PartialRoundLimit) { 1 } else { 2 }, 2,
    ).unwrap();
    let (initial, operation) = pair(
        peer.dispatch_driver(&cx, 1, reviewed, Delivery::Complete),
        tool.start_interaction_with_cancellation(&cx, &cancellation, original(), RequestId::Number(1), limits),
    ).await;
    let mut operation = operation.unwrap();
    assert_eq!(initial["result"]["resultType"], "input_required");
    let callbacks = AtomicUsize::new(0);
    let polls = AtomicUsize::new(0);
    let resolved = AtomicUsize::new(0);
    let dropped = Arc::new(AtomicBool::new(false));
    let states = Mutex::new(Vec::<String>::new());
    let mut previous_replies = Vec::new();
    if matches!(case, Case::Observed | Case::AlreadyComplete) {
        assert!(matches!(operation.next_event(&cx).await.unwrap(), Some(ManagedInteractionEvent::InputRequired(_))));
    }
    if matches!(case, Case::AlreadyComplete) {
        let supplied = answers(&["left", "right"], &resolved);
        let (wire, sent) = pair(
            peer.dispatch_driver(&cx, 2, reviewed, Delivery::Complete),
            operation.resume(&cx, RequestId::Number(2), Some(supplied)),
        ).await;
        sent.unwrap();
        assert!(matches!(operation.next_event(&cx).await.unwrap(), Some(ManagedInteractionEvent::Complete(_))));
        previous_replies.push(wire);
    }
    if matches!(case, Case::PreInvalidated) { tool.invalidate(); }

    let resolve = |input: Box<InputRequiredResult>| {
        let round = callbacks.fetch_add(1, Ordering::SeqCst);
        if matches!(case, Case::InvalidateConstructor) { tool.invalidate(); }
        let dropped = dropped.clone();
        let tool = &tool;
        let cancellation = &cancellation;
        let polls = &polls;
        let resolved = &resolved;
        let states = &states;
        async move {
            polls.fetch_add(1, Ordering::SeqCst);
            let requests = input.input_requests().unwrap();
            assert_eq!(requests.members().len(), if case.partial() && round > 0 { 1 } else { 2 });
            assert!(requests.get("right").is_some());
            if case.partial() && round > 0 { assert!(requests.get("left").is_none()); }
            states.lock().unwrap().push(input.request_state().unwrap().to_owned());
            if case.pending() {
                let _owned = ResolutionGuard(dropped);
                return std::future::pending::<Result<ManagedInputReply, ManagedInteractionError>>().await;
            }
            if matches!(case, Case::Decline) { return Err(ManagedInteractionError::AbortedByHost); }
            let keys: &[&str] = if matches!(case, Case::WrongAnswer) { &["foreign"] }
                else if case.partial() { if round == 0 { &["left"] } else { &["right"] } }
                else { &["left", "right"] };
            let responses = answers(keys, resolved);
            if matches!(case, Case::InvalidateReady) { tool.invalidate(); }
            if matches!(case, Case::CancelReady) { cancellation.cancel(); }
            Ok(ManagedInputReply {
                request_id: RequestId::Number(if matches!(case, Case::RepeatedId) { 1 } else { 2 + i64::try_from(round).unwrap() }),
                input_responses: Some(responses),
            })
        }
    };
    let drive = async {
        if case.partial() {
            operation.drive_partial(&cx, resolve, |_| panic!("no notifications were requested")).await
        } else {
            operation.drive(&cx, resolve, |_| panic!("no notifications were requested")).await
        }
    };
    let (replies, result) = if case.pending() {
        let mut drive = Box::pin(drive);
        poll_fn(|task| {
            assert!(drive.as_mut().poll(task).is_pending(), "a pending resolver must not finish");
            if polls.load(Ordering::SeqCst) > 0 { Poll::Ready(()) } else { Poll::Pending }
        }).await;
        if matches!(case, Case::AbandonPending) {
            drop(drive);
            (Vec::new(), None)
        } else {
            if matches!(case, Case::CancelPending) { cancellation.cancel(); } else { tool.invalidate(); }
            (Vec::new(), Some(drive.await))
        }
    } else {
        let server = async {
            let mut replies = previous_replies;
            for index in replies.len()..case.continuations() {
                let delivery = if matches!(case, Case::LostReply) { Delivery::LoseHead } else { Delivery::Complete };
                replies.push(peer.dispatch_driver(&cx, 2 + i64::try_from(index).unwrap(), reviewed, delivery).await);
            }
            replies
        };
        let (replies, result) = pair(server, drive).await;
        (replies, Some(result))
    };

    match (case, result) {
        (Case::Complete | Case::NoHeaders | Case::Partial | Case::Observed, Some(Ok(result))) => {
            let actual: Value = serde_json::from_str(&result.encode().unwrap()).unwrap();
            assert_eq!(actual, replies.last().unwrap()["result"]);
            assert_eq!(actual["structuredContent"]["quantity"], 1);
            assert_eq!(actual["structuredContent"]["effect"], 1);
            assert_eq!(actual["structuredContent"]["order"], json!(["left", "right"]));
        }
        (Case::InvalidOutput | Case::PartialInvalidOutput,
            Some(Err(ManagedToolInteractionError::Tool(ManagedToolError::InvalidStructuredOutput)))) => {}
        (Case::InvalidateConstructor | Case::InvalidateReady | Case::InvalidatePending | Case::PreInvalidated,
            Some(Err(ManagedToolInteractionError::Tool(ManagedToolError::Invalidated)))) => {}
        (Case::CancelReady | Case::CancelPending,
            Some(Err(ManagedToolInteractionError::Tool(ManagedToolError::Core(ManagedCoreError::Cancelled))))) => {}
        (Case::Decline, Some(Err(ManagedToolInteractionError::Interaction(ManagedInteractionError::AbortedByHost)))) => {}
        (Case::WrongAnswer, Some(Err(ManagedToolInteractionError::Interaction(ManagedInteractionError::InvalidInputResponses)))) => {}
        (Case::RepeatedId, Some(Err(ManagedToolInteractionError::Interaction(ManagedInteractionError::RepeatedRequestId)))) => {}
        (Case::PartialRoundLimit, Some(Err(ManagedToolInteractionError::Interaction(ManagedInteractionError::ContinuationLimit)))) => {}
        (Case::AlreadyComplete, Some(Err(ManagedToolInteractionError::Tool(ManagedToolError::Closed)))) => {}
        (Case::LostReply, Some(Err(_))) => {}
        (Case::AbandonPending, None) => {}
        _ => panic!("unexpected schema-bound driver outcome for {case:?}"),
    }
    let expected_callbacks = match case {
        Case::PreInvalidated | Case::AlreadyComplete => 0,
        Case::Partial | Case::PartialInvalidOutput => 2,
        _ => 1,
    };
    let expected_answers = match case {
        Case::Decline | Case::PreInvalidated | Case::InvalidateConstructor
            | Case::InvalidatePending | Case::CancelPending | Case::AbandonPending => 0,
        Case::WrongAnswer | Case::PartialRoundLimit => 1,
        _ => 2,
    };
    let completes = matches!(case, Case::Complete | Case::NoHeaders | Case::Partial | Case::Observed
        | Case::InvalidOutput | Case::PartialInvalidOutput | Case::LostReply | Case::AlreadyComplete);
    assert_eq!(callbacks.load(Ordering::SeqCst), expected_callbacks);
    assert_eq!(polls.load(Ordering::SeqCst), expected_callbacks - usize::from(matches!(case, Case::InvalidateConstructor)));
    assert_eq!(resolved.load(Ordering::SeqCst), expected_answers);
    assert_eq!(peer.probe.starts.load(Ordering::SeqCst), 1);
    assert_eq!(peer.probe.effects.load(Ordering::SeqCst), usize::from(completes));
    assert_eq!(peer.probe.transforms.load(Ordering::SeqCst), usize::from(completes));
    if case.pending() { assert!(dropped.load(Ordering::SeqCst)); }
    let observed_states = states.lock().unwrap();
    assert_eq!(observed_states.len(), polls.load(Ordering::SeqCst));
    for (index, state) in observed_states.iter().enumerate() {
        let previous = if index == 0 { &initial } else { &replies[index - 1] };
        assert_eq!(Some(state.as_str()), previous["result"]["requestState"].as_str());
        assert_ne!(state, "handler-private-state");
    }
    drop(observed_states);
    let requests = peer.seen.lock().unwrap();
    assert_eq!(requests.len(), 1 + case.continuations());
    for (index, request) in requests.iter().enumerate().skip(1) {
        let previous = if index == 1 { &initial } else { &replies[index - 2] };
        assert_eq!(request["params"]["requestState"], previous["result"]["requestState"]);
        assert_eq!(request["params"]["arguments"], requests[0]["params"]["arguments"]);
        assert_eq!(request["params"]["_meta"], requests[0]["params"]["_meta"]);
        assert_eq!(request["id"], 1 + i64::try_from(index).unwrap());
        let response_keys: Vec<_> = request["params"]["inputResponses"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(response_keys, if case.partial() { if index == 1 { vec!["left"] } else { vec!["right"] } } else { vec!["left", "right"] });
    }
    drop(requests);
    peer.quiet();
    assert_eq!(peer.grants.load(Ordering::SeqCst), 1);
    assert!(cx.checkpoint().is_ok());
    // Failed, invalidated or abandoned driver custody must not cancel its login.
    let metadata = original().encode_params().unwrap().unwrap()["_meta"].clone();
    let list = CoreRequest::decode(ProtocolEra::Modern2026, "tools/list", Some(&json!({"_meta":metadata}))).unwrap();
    let sibling = async {
        let mut call = session.request_core(&cx, list, RequestId::Number(900), ManagedCoreLimits::default()).await.unwrap();
        assert!(matches!(call.next_event(&cx).await.unwrap(), Some(ManagedCoreEvent::Result(_))));
    };
    pair(peer.dispatch(&cx, Delivery::Complete), sibling).await;
    assert_eq!(peer.seen.lock().unwrap().len(), 2 + case.continuations());
    assert_eq!(peer.grants.load(Ordering::SeqCst), 1);
    peer.quiet();
    session.close();
    peer.journal.close().unwrap();
}

fn execute(name: &str, case: Case) {
    super::isolated(&format!("schema_driver::{name}"), super::Case::SchemaDriver(case));
}

#[test] fn complete_inputs_retain_schema_and_reviewed_headers() { execute("complete_inputs_retain_schema_and_reviewed_headers", Case::Complete); }
#[test] fn annotations_without_review_remain_body_only() { execute("annotations_without_review_remain_body_only", Case::NoHeaders); }
#[test] fn partial_inputs_keep_headers_and_resolve_each_key_once() { execute("partial_inputs_keep_headers_and_resolve_each_key_once", Case::Partial); }
#[test] fn an_observed_challenge_can_enter_the_driver() { execute("an_observed_challenge_can_enter_the_driver", Case::Observed); }
#[test] fn invalid_terminal_output_is_not_published() { execute("invalid_terminal_output_is_not_published", Case::InvalidOutput); }
#[test] fn partial_workflow_cannot_bypass_output_validation() { execute("partial_workflow_cannot_bypass_output_validation", Case::PartialInvalidOutput); }
#[test] fn host_decline_never_posts_a_continuation() { execute("host_decline_never_posts_a_continuation", Case::Decline); }
#[test] fn wrong_answer_key_never_posts_a_continuation() { execute("wrong_answer_key_never_posts_a_continuation", Case::WrongAnswer); }
#[test] fn reused_request_id_never_posts_a_continuation() { execute("reused_request_id_never_posts_a_continuation", Case::RepeatedId); }
#[test] fn constructor_invalidation_prevents_resolver_polling() { execute("constructor_invalidation_prevents_resolver_polling", Case::InvalidateConstructor); }
#[test] fn ready_answer_invalidation_prevents_dispatch() { execute("ready_answer_invalidation_prevents_dispatch", Case::InvalidateReady); }
#[test] fn ready_answer_cancellation_prevents_dispatch() { execute("ready_answer_cancellation_prevents_dispatch", Case::CancelReady); }
#[test] fn invalidation_ends_a_pending_resolver() { execute("invalidation_ends_a_pending_resolver", Case::InvalidatePending); }
#[test] fn cancellation_ends_a_pending_resolver() { execute("cancellation_ends_a_pending_resolver", Case::CancelPending); }
#[test] fn abandoning_a_pending_resolver_drops_its_work() { execute("abandoning_a_pending_resolver_drops_its_work", Case::AbandonPending); }
#[test] fn partial_round_budget_prevents_another_host_callback() { execute("partial_round_budget_prevents_another_host_callback", Case::PartialRoundLimit); }
#[test] fn retired_tool_never_enters_the_host() { execute("retired_tool_never_enters_the_host", Case::PreInvalidated); }
#[test] fn lost_reply_is_not_retried_or_resolved_again() { execute("lost_reply_is_not_retried_or_resolved_again", Case::LostReply); }
#[test] fn a_delivered_result_cannot_be_driven_again() { execute("a_delivered_result_cannot_be_driven_again", Case::AlreadyComplete); }
