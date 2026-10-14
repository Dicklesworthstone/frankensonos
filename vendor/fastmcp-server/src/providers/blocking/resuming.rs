//! The real pool boundary, typed input registry and router remain in these tests.

use super::*;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use fastmcp_core::SessionState;
use fastmcp_protocol::{
    ClientCapabilities, CoreRequest, CoreResult, FinalCoreResult, FinalRequestMeta,
    InputRequiredResult, JsonRpcRequest,
};
use fastmcp_protocol::protocol_policy::ProtocolEra;
use serde_json::json;
use crate::bidirectional::{MrtrExchangeRegistry, MrtrInputRequest, MrtrInputRequests, MrtrRetry};
use crate::router::Router;

fn runtime(pool: bool) -> asupersync::runtime::Runtime {
    let builder = asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap());
    if pool { builder.blocking_threads(1, 2).build().unwrap() }
    else { builder.build().unwrap() }
}

fn context(cx: &Cx, id: u64) -> McpContext {
    McpContext::new(cx.clone(), id).with_operation_deadline(Some(
        cx.now().saturating_add_nanos(5_000_000_000),
    ))
}

fn core_request() -> CoreRequest {
    CoreRequest::decode(ProtocolEra::Modern2026, "tools/call", Some(&json!({
        "name":"resuming_blocking", "arguments":{"value":7},
        "_meta":FinalRequestMeta::new(ClientCapabilities::default()),
    }))).unwrap()
}

fn required(key: &str) -> InputRequiredResult {
    let wire = json!({"resultType":"input_required", "requestState":"",
        "inputRequests":{key:{"method":"roots/list"}}}).to_string();
    let CoreResult::Final(FinalCoreResult::ToolsCallInputRequired { result, .. }) =
        core_request().decode_result(&wire).unwrap()
    else { panic!("typed input-required fixture") };
    result
}

fn complete() -> CompleteResult<FinalCallToolResult> {
    let CoreResult::Final(FinalCoreResult::ToolsCall { result, .. }) = core_request().decode_result(
        r#"{"resultType":"complete","content":[{"type":"text","text":"completed after input"}],"_meta":{"com.example/revision":3},"x-exact":{"n":900719925474099312345,"decimal":1.20e+4}}"#,
    ).unwrap() else { panic!("typed complete fixture") };
    result
}

fn encoded_complete(result: CompleteResult<FinalCallToolResult>) -> String {
    CoreResult::Final(FinalCoreResult::ToolsCall { result, diagnostic: None }).encode().unwrap()
}

fn accepted(ctx: &McpContext, key: &str, name: &str) -> MrtrCompletedInputs {
    let registry = MrtrExchangeRegistry::new();
    let issued = registry.issue(ctx.request_cancellation().clone(),
        MrtrInputRequests::new([(key.to_owned(), MrtrInputRequest::roots())]).unwrap(),
    ).unwrap();
    let wire = serde_json::to_value(issued).unwrap();
    let responses = BTreeMap::from([(key.to_owned(), json!({"roots":[{
        "uri":"file:///workspace", "name":name,
    }]}))]);
    let MrtrRetry::Complete(inputs) = registry.accept_wire(
        wire["requestState"].as_str().unwrap(), &responses,
    ).unwrap() else { panic!("the actual registry must admit a complete roots response") };
    inputs
}

struct ToolFixture {
    calls: Arc<AtomicUsize>,
    legacy_calls: Arc<AtomicUsize>,
    poller: std::thread::ThreadId,
    asynchronous: bool,
    declared: bool,
}

impl ToolFixture {
    fn new() -> Self {
        Self { calls: Arc::new(AtomicUsize::new(0)), legacy_calls: Arc::new(AtomicUsize::new(0)),
            poller: std::thread::current().id(), asynchronous: false, declared: true }
    }

    fn resume(&self, ctx: &McpContext, arguments: Value, inputs: Option<&MrtrCompletedInputs>)
        -> McpResult<FinalToolOutcome>
    {
        assert_ne!(std::thread::current().id(), self.poller);
        assert_eq!(Cx::current().unwrap().task_id(), ctx.task_id());
        assert_eq!(arguments, json!({"value":7}));
        self.calls.fetch_add(1, Ordering::SeqCst);
        if let Some(inputs) = inputs {
            if let Some(roots) = inputs.roots("second")? {
                assert_eq!(roots.roots[0].name.as_deref(), Some("second value"));
                return Ok(FinalToolOutcome::Complete(complete()));
            }
            let roots = inputs.roots("first")?.expect("first-round typed input");
            assert_eq!(roots.roots[0].name.as_deref(), Some("first value"));
            return Ok(FinalToolOutcome::InputRequired(required("second")));
        }
        Ok(FinalToolOutcome::InputRequired(required("first")))
    }
}

impl ToolHandler for ToolFixture {
    fn definition(&self) -> Tool {
        Tool { name:"resuming_blocking".into(), description:Some("Resumable tool".into()),
            input_schema:json!({"type":"object","required":["value"],
                "properties":{"value":{"type":"integer"}},"additionalProperties":false}),
            output_schema:None, icon:None, version:None, tags:vec!["interactive".into()], annotations:None }
    }
    fn final_title(&self) -> Option<&str> { Some("Resumable blocking tool") }
    fn timeout(&self) -> Option<Duration> { Some(Duration::from_secs(5)) }
    fn execution_mode(&self) -> ToolExecutionMode {
        if self.asynchronous { ToolExecutionMode::Async } else { ToolExecutionMode::Blocking }
    }
    fn declares_final_mrtr(&self) -> bool { self.declared }
    fn call(&self, _ctx: &McpContext, _arguments: Value) -> McpResult<Vec<Content>> {
        assert_ne!(std::thread::current().id(), self.poller);
        self.legacy_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Content::Text { text:"legacy only".into() }])
    }
    fn call_final(&self, _ctx: &McpContext, _arguments: Value)
        -> McpResult<CompleteResult<FinalCallToolResult>>
    { panic!("resumable final work must not fall back to a complete-only hook") }
}

#[test]
fn tool_runs_initial_and_multiple_resumed_rounds_without_holding_a_worker_between_rounds() {
    runtime(true).block_on(async {
        let cx = Cx::current().unwrap();
        let ctx = context(&cx, 71);
        let lane = BlockingHandlerLane::new(1).unwrap();
        let handler = ToolFixture::new();
        let calls = Arc::clone(&handler.calls);
        let legacy = Arc::clone(&handler.legacy_calls);
        let tool = BlockingTool::from_sync_resuming_hook(handler, lane.clone(), ToolFixture::resume).unwrap();
        assert!(tool.declares_final_mrtr());
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Async);
        assert_eq!(tool.final_title(), Some("Resumable blocking tool"));
        assert_eq!(tool.timeout(), Some(Duration::from_secs(5)));
        assert!(matches!(tool.call_final_outcome_async(&ctx, json!({"value":7})).await,
            Outcome::Ok(FinalToolOutcome::InputRequired(_))));
        assert_eq!(lane.in_flight().unwrap(), 0);
        let first = accepted(&ctx, "first", "first value");
        assert!(matches!(tool.call_final_outcome_async_resuming_in_request(
            &context(&cx, 72), &cx, json!({"value":7}), Some(&first),
        ).await, Outcome::Ok(FinalToolOutcome::InputRequired(_))));
        assert_eq!(lane.in_flight().unwrap(), 0);
        let second = accepted(&ctx, "second", "second value");
        let Outcome::Ok(FinalToolOutcome::Complete(result)) = tool.call_final_outcome_async_resuming_in_request(
            &context(&cx, 73), &cx, json!({"value":7}), Some(&second),
        ).await else { panic!("third round must return the exact complete result") };
        assert_eq!(encoded_complete(result), encoded_complete(complete()));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(legacy.load(Ordering::SeqCst), 0);
        assert_eq!(lane.in_flight().unwrap(), 0);
        assert_eq!(first.roots("first").unwrap().unwrap().roots[0].name.as_deref(), Some("first value"));
        assert!(matches!(tool.call_async(&ctx, json!({})).await, Outcome::Ok(_)));
        assert_eq!(legacy.load(Ordering::SeqCst), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    });
}

#[test]
fn tool_requires_explicit_sync_selection_and_never_runs_a_resume_hook_on_complete_only_entries() {
    let lane = BlockingHandlerLane::new(1).unwrap();
    assert!(BlockingTool::new(ToolFixture::new(), lane.clone()).is_err());
    let mut asynchronous = ToolFixture::new();
    asynchronous.asynchronous = true;
    assert!(BlockingTool::from_sync_resuming_hook(asynchronous, lane.clone(), ToolFixture::resume).is_err());
    runtime(true).block_on(async {
        let ctx = context(&Cx::current().unwrap(), 71);
        let mut handler = ToolFixture::new();
        handler.declared = false; // The explicit selected hook establishes the declaration.
        let calls = Arc::clone(&handler.calls);
        let legacy = Arc::clone(&handler.legacy_calls);
        let tool = BlockingTool::from_sync_resuming_hook(handler, lane, ToolFixture::resume).unwrap();
        assert!(tool.declares_final_mrtr());
        assert!(tool.call(&ctx, json!({})).is_err());
        assert!(tool.call_final(&ctx, json!({})).is_err());
        assert!(matches!(tool.call_final_async(&ctx, json!({})).await, Outcome::Err(_)));
        assert!(matches!(tool.call_final_async_in_request(&ctx, ctx.cx(), json!({})).await, Outcome::Err(_)));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(legacy.load(Ordering::SeqCst), 0);
    });
}

#[test]
fn tool_resume_rejects_missing_pool_closed_lane_and_cancelled_requests_before_effects() {
    for case in 0..3 {
        runtime(case != 0).block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = context(&cx, 71);
            let inputs = accepted(&ctx, "first", "first value");
            let lane = BlockingHandlerLane::new(1).unwrap();
            if case == 1 { lane.close().unwrap(); }
            if case == 2 { ctx.request_cancellation().cancel(); }
            let handler = ToolFixture::new();
            let calls = Arc::clone(&handler.calls);
            let tool = BlockingTool::from_sync_resuming_hook(handler, lane.clone(), ToolFixture::resume).unwrap();
            assert!(matches!(tool.call_final_outcome_async_in_request(&ctx, &cx, json!({"value":7})).await,
                Outcome::Err(_)));
            assert!(matches!(tool.call_final_outcome_async_resuming_in_request(
                &ctx, &cx, json!({"value":7}), Some(&inputs),
            ).await, Outcome::Err(_)));
            assert_eq!(calls.load(Ordering::SeqCst), 0);
            assert_eq!(lane.in_flight().unwrap(), 0);
            assert!(inputs.roots("first").unwrap().is_some());
        });
    }
}

#[test]
fn abandoned_tool_resume_keeps_inputs_and_capacity_until_the_real_worker_finishes() {
    runtime(true).block_on(async {
        let cx = Cx::current().unwrap();
        let ctx = context(&cx, 71);
        let peer = context(&cx, 72);
        let inputs = accepted(&ctx, "first", "first value");
        let lane = BlockingHandlerLane::new(1).unwrap();
        let (started, mut entered) = oneshot::channel::<()>();
        let started = Mutex::new(Some(started));
        let (release, blocked) = std::sync::mpsc::sync_channel::<()>(1);
        let blocked = Mutex::new(blocked);
        let retained = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&retained);
        let tool = BlockingTool::from_sync_resuming_hook(ToolFixture::new(), lane.clone(), move |_, ctx, _, inputs| {
            started.lock().unwrap().take().unwrap().send_blocking(()).unwrap();
            blocked.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
            let roots = inputs.unwrap().roots("first")?.unwrap();
            *observed.lock().unwrap() = roots.roots[0].name.clone();
            assert!(ctx.ensure_live().is_err(), "abandoned request aborts only this worker");
            Ok(FinalToolOutcome::Complete(complete()))
        }).unwrap();
        let mut call = tool.call_final_outcome_async_resuming_in_request(
            &ctx, &cx, json!({"value":7}), Some(&inputs),
        );
        poll_fn(|task| { assert!(call.as_mut().poll(task).is_pending()); Poll::Ready(()) }).await;
        entered.recv(&cx).await.unwrap();
        drop(call);
        drop(inputs); // The admitted worker must own its copy after this point.
        assert_eq!(lane.in_flight().unwrap(), 1);
        assert!(lane.execute(&peer, &cx, |_| Ok(())).await.is_err());
        release.send(()).unwrap();
        lane.wait_idle(&peer).await.unwrap();
        assert_eq!(retained.lock().unwrap().as_deref(), Some("first value"));
        assert_eq!(lane.in_flight().unwrap(), 0);
        assert_eq!(lane.execute(&peer, &cx, |_| Ok(42)).await.unwrap(), 42);
        assert!(ctx.ensure_live().is_ok());
        assert!(peer.ensure_live().is_ok());
    });
}

#[test]
fn tool_resume_panic_is_redacted_and_does_not_poison_the_shared_lane() {
    runtime(true).block_on(async {
        let cx = Cx::current().unwrap();
        let ctx = context(&cx, 71);
        let inputs = accepted(&ctx, "first", "first value");
        let lane = BlockingHandlerLane::new(1).unwrap();
        let tool = BlockingTool::from_sync_resuming_hook(ToolFixture::new(), lane.clone(), |_, _, _, _| {
            panic!("private-resume-panic-canary")
        }).unwrap();
        let Outcome::Err(error) = tool.call_final_outcome_async_resuming_in_request(
            &ctx, &cx, json!({"value":7}), Some(&inputs),
        ).await else { panic!("panicking resumption must fail") };
        assert!(!error.to_string().contains("private-resume-panic-canary"));
        assert_eq!(lane.in_flight().unwrap(), 0);
        assert_eq!(lane.execute(&ctx, &cx, |_| Ok(43)).await.unwrap(), 43);
    });
}

async fn route(router: &Arc<Router>, state: &SessionState, id: i64, params: Value) -> McpResult<Value> {
    route_with_owner(router, state, id, params, true).await
}

/// Modern transport admission marks each request as the owner of its retained
/// continuations (`Router`'s request context does this); the router refuses an
/// MRTR-capable handler for a context without that binding.
async fn route_with_owner(
    router: &Arc<Router>, state: &SessionState, id: i64, params: Value, owner: bool,
) -> McpResult<Value> {
    let cx = Cx::current().unwrap();
    let ctx = McpContext::with_state(cx.clone(), id as u64, state.clone())
        .with_operation_deadline(Some(cx.now().saturating_add_nanos(5_000_000_000)));
    let ctx = if owner { ctx.with_retained_continuation_owner() } else { ctx };
    router.clone().dispatch_stateless_owned(ctx, JsonRpcRequest::new("tools/call", Some(params), id)).await
}

fn params() -> Value {
    json!({"name":"resuming_blocking", "arguments":{"value":7}, "_meta":{
        "io.modelcontextprotocol/protocolVersion":"2026-07-28",
        "io.modelcontextprotocol/clientCapabilities":{"roots":{}},
    }})
}

#[test]
fn router_owns_multi_round_tokens_and_refuses_wrong_inputs_without_running_the_hook() {
    runtime(true).block_on(async {
        let lane = BlockingHandlerLane::new(1).unwrap();
        let handler = ToolFixture::new();
        let calls = Arc::clone(&handler.calls);
        let tool = BlockingTool::from_sync_resuming_hook(handler, lane.clone(), ToolFixture::resume).unwrap();
        let mut router = Router::new();
        router.add_tool(tool).unwrap();
        let router = Arc::new(router);
        let state = SessionState::new();
        let unbound = route_with_owner(&router, &state, 0, params(), false).await.unwrap_err();
        assert_eq!(unbound.message, "MRTR-capable handlers require a bound modern connection");
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        let first = route(&router, &state, 1, params()).await.unwrap();
        assert_eq!(first["resultType"], "input_required");
        assert_eq!(first["inputRequests"]["first"]["method"], "roots/list");
        assert!(!first["requestState"].as_str().unwrap().is_empty());
        assert_eq!(lane.in_flight().unwrap(), 0);
        let mut retry = params();
        retry["requestState"] = first["requestState"].clone();
        retry["inputResponses"] = json!({"first":{"roots":[{"uri":"file:///workspace","name":"first value"}]}});
        let mut wrong = retry.clone();
        wrong["inputResponses"] = json!({"first":{"action":"accept","content":{}}});
        assert!(route(&router, &state, 2, wrong).await.is_err());
        let mut wrong = retry.clone();
        wrong["arguments"]["value"] = json!(8);
        assert!(route(&router, &state, 3, wrong).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let second = route(&router, &state, 4, retry.clone()).await.unwrap();
        assert_eq!(second["resultType"], "input_required");
        assert_ne!(second["requestState"], first["requestState"]);
        assert_eq!(second["inputRequests"]["second"]["method"], "roots/list");
        assert!(route(&router, &state, 5, retry).await.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let mut final_retry = params();
        final_retry["requestState"] = second["requestState"].clone();
        final_retry["inputResponses"] = json!({"second":{"roots":[{"uri":"file:///workspace","name":"second value"}]}});
        let result = route(&router, &state, 6, final_retry).await.unwrap();
        assert_eq!(result["resultType"], "complete");
        assert_eq!(result["content"][0]["text"], "completed after input");
        assert_eq!(result["_meta"]["com.example/revision"], 3);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(lane.in_flight().unwrap(), 0);
    });
}
