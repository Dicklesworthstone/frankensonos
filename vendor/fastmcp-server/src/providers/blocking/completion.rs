//! Blocking completion callbacks on the same bounded caller-owned lane.

use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult};
use fastmcp_protocol::{
    CompletionValues, FinalCompletionParams, FinalCompletionValues, LegacyCompletionParams,
};

use super::{BlockingHandlerLane, outcome};
use crate::handler::{BoxFuture, CompletionHandler};

/// Runs explicitly selected synchronous completion callbacks on the caller pool.
///
/// Register through the existing prompt/resource-template completion APIs, or
/// the explicitly configured fallback. Reference routing and authorization stay
/// with the router; this adapter neither adds a route nor rewrites references,
/// titles, metadata, argument values, or completion context. The exact legacy
/// and modern callback/result types remain separate.
///
/// Construction deliberately selects synchronous hooks. It does not detect
/// async overrides, and must not wrap an async-only provider or another blocking
/// adapter. There is no inline execution or privately created runtime. Share the
/// lane with other blocking handlers to bound their combined outstanding work.
pub struct BlockingCompletion<H> {
    handler: Arc<H>,
    lane: BlockingHandlerLane,
}

impl<H: CompletionHandler + 'static> BlockingCompletion<H> {
    /// Selects `complete_legacy` and `complete_final`; performs no I/O.
    pub fn from_sync_hooks(handler: H, lane: BlockingHandlerLane) -> Self {
        Self {
            handler: Arc::new(handler),
            lane,
        }
    }
}

fn async_required() -> McpError {
    McpError::invalid_request("blocking completion requires asynchronous caller-owned dispatch")
}

impl<H: CompletionHandler + 'static> CompletionHandler for BlockingCompletion<H> {
    fn timeout(&self) -> Option<Duration> {
        self.handler.timeout()
    }
    fn complete_legacy(
        &self,
        _ctx: &McpContext,
        _params: LegacyCompletionParams,
    ) -> McpResult<CompletionValues> {
        Err(async_required())
    }
    fn complete_final(
        &self,
        _ctx: &McpContext,
        _params: FinalCompletionParams,
    ) -> McpResult<FinalCompletionValues> {
        Err(async_required())
    }
    fn complete_legacy_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        params: LegacyCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<CompletionValues>> {
        self.complete_legacy_async_in_request(ctx, ctx.cx(), params)
    }
    fn complete_legacy_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        params: LegacyCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<CompletionValues>> {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(
                self.lane
                    .execute(ctx, cx, move |ctx| handler.complete_legacy(ctx, params))
                    .await,
            )
        })
    }
    fn complete_final_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        params: FinalCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<FinalCompletionValues>> {
        self.complete_final_async_in_request(ctx, ctx.cx(), params)
    }
    fn complete_final_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        params: FinalCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<FinalCompletionValues>> {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(
                self.lane
                    .execute(ctx, cx, move |ctx| handler.complete_final(ctx, params))
                    .await,
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{BlockingPrompt, BlockingResource};
    use super::*;
    use crate::handler::{PromptHandler, ResourceHandler};
    use asupersync::channel::oneshot;
    use fastmcp_core::Outcome;
    use fastmcp_protocol::{Prompt, PromptMessage, Resource, ResourceContent};
    use serde_json::{Value, json};
    use std::collections::HashMap;
    use std::future::{Future, poll_fn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;

    type Calls = Arc<Mutex<Vec<(&'static str, Value)>>>;
    struct Completer {
        calls: Calls,
        poller: std::thread::ThreadId,
    }
    impl Completer {
        fn record(&self, ctx: &McpContext, era: &'static str, params: Value) {
            assert_ne!(std::thread::current().id(), self.poller);
            assert_eq!(ctx.request_id(), 97);
            assert_eq!(Cx::current().unwrap().task_id(), ctx.task_id());
            self.calls.lock().unwrap().push((era, params));
        }
    }
    impl CompletionHandler for Completer {
        fn timeout(&self) -> Option<Duration> {
            Some(Duration::from_secs(2))
        }
        fn complete_legacy(
            &self,
            ctx: &McpContext,
            params: LegacyCompletionParams,
        ) -> McpResult<CompletionValues> {
            self.record(ctx, "legacy", serde_json::to_value(params).unwrap());
            Ok(serde_json::from_value(json!({
                "values": ["legacy-value"],
                "total": 1,
                "hasMore": false
            }))
            .unwrap())
        }
        fn complete_final(
            &self,
            ctx: &McpContext,
            params: FinalCompletionParams,
        ) -> McpResult<FinalCompletionValues> {
            let cancel = params.argument.value == "cancel";
            self.record(ctx, "final", serde_json::to_value(params).unwrap());
            if cancel {
                ctx.request_cancellation().cancel();
            }
            Ok(serde_json::from_value(json!({
                "values": ["final-value"],
                "total": 7,
                "hasMore": true
            }))
            .unwrap())
        }
    }
    fn legacy_params() -> LegacyCompletionParams {
        serde_json::from_value(json!({
            "ref": {"type": "ref/prompt", "name": "render"},
            "argument": {"name": "body", "value": "alpha"},
            "_meta": {"progressToken": "p-97"}
        }))
        .unwrap()
    }
    fn final_params() -> FinalCompletionParams {
        serde_json::from_value(json!({
            "ref": {"type": "ref/prompt", "name": "render", "title": "Exact title"},
            "argument": {"name": "body", "value": "α\nβ"},
            "context": {"arguments": {"style": "formal", "_meta": "ordinary context value"}},
            "_meta": {"com.example/context": "kept"}
        }))
        .unwrap()
    }
    fn runtime(pool: bool) -> asupersync::runtime::Runtime {
        let builder = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap());
        if pool {
            builder.blocking_threads(0, 2).build().unwrap()
        } else {
            builder.build().unwrap()
        }
    }
    fn fixture(lane: BlockingHandlerLane) -> (BlockingCompletion<Completer>, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            BlockingCompletion::from_sync_hooks(
                Completer {
                    calls: Arc::clone(&calls),
                    poller: std::thread::current().id(),
                },
                lane,
            ),
            calls,
        )
    }

    #[test]
    fn completion_preserves_both_eras_and_all_async_entry_points_on_the_pool() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 97);
            let (completion, calls) = fixture(BlockingHandlerLane::new(1).unwrap());
            let completion: &dyn CompletionHandler = &completion;
            assert_eq!(completion.timeout(), Some(Duration::from_secs(2)));
            assert!(completion.complete_legacy(&ctx, legacy_params()).is_err());
            assert!(completion.complete_final(&ctx, final_params()).is_err());
            assert!(calls.lock().unwrap().is_empty());
            for result in [
                completion.complete_legacy_async(&ctx, legacy_params()).await,
                completion
                    .complete_legacy_async_in_request(&ctx, ctx.cx(), legacy_params())
                    .await,
            ] {
                let Outcome::Ok(result) = result else {
                    panic!("legacy completion expected")
                };
                assert_eq!(
                    serde_json::to_value(result).unwrap(),
                    json!({"values": ["legacy-value"], "total": 1, "hasMore": false})
                );
            }
            for result in [
                completion.complete_final_async(&ctx, final_params()).await,
                completion
                    .complete_final_async_in_request(&ctx, ctx.cx(), final_params())
                    .await,
            ] {
                let Outcome::Ok(result) = result else {
                    panic!("final completion expected")
                };
                assert_eq!(
                    serde_json::to_value(result).unwrap(),
                    json!({"values": ["final-value"], "total": 7, "hasMore": true})
                );
            }
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 4);
            assert_eq!(
                calls[0],
                ("legacy", serde_json::to_value(legacy_params()).unwrap())
            );
            assert_eq!(calls[1], calls[0]);
            assert_eq!(
                calls[2],
                ("final", serde_json::to_value(final_params()).unwrap())
            );
            assert_eq!(calls[3], calls[2]);
        });
    }

    #[test]
    fn completion_keeps_context_absence_empty_and_populated_states_distinct() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 97);
            let (completion, calls) = fixture(BlockingHandlerLane::new(1).unwrap());
            for context in [
                None,
                Some(json!({})),
                Some(json!({"arguments": {}})),
                Some(json!({"arguments": {"body": "x"}})),
            ] {
                let mut wire = serde_json::to_value(final_params()).unwrap();
                wire.as_object_mut().unwrap().remove("context");
                if let Some(context) = context {
                    wire["context"] = context;
                }
                let params: FinalCompletionParams = serde_json::from_value(wire.clone()).unwrap();
                assert!(matches!(
                    completion.complete_final_async(&ctx, params).await,
                    Outcome::Ok(_)
                ));
                assert_eq!(calls.lock().unwrap().last().unwrap().1, wire);
            }
            assert_eq!(calls.lock().unwrap().len(), 4);
        });
    }

    #[test]
    fn unavailable_pool_closed_lane_and_precancellation_never_invoke_completions() {
        for case in 0..3 {
            runtime(case != 0).block_on(async {
                let ctx = McpContext::new(Cx::current().unwrap(), 97);
                let lane = BlockingHandlerLane::new(1).unwrap();
                if case == 1 {
                    lane.close().unwrap();
                }
                if case == 2 {
                    ctx.request_cancellation().cancel();
                }
                let (completion, calls) = fixture(lane.clone());
                assert!(matches!(
                    completion.complete_legacy_async(&ctx, legacy_params()).await,
                    Outcome::Err(_)
                ));
                assert!(matches!(
                    completion.complete_final_async(&ctx, final_params()).await,
                    Outcome::Err(_)
                ));
                assert!(calls.lock().unwrap().is_empty());
                assert_eq!(lane.in_flight().unwrap(), 0);
            });
        }
    }

    #[test]
    fn completion_deadline_before_dispatch_and_cancellation_during_return_withhold_results() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 97);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let (completion, calls) = fixture(lane.clone());
            let expired = McpContext::new(ctx.cx().clone(), 97)
                .with_operation_deadline(Some(ctx.cx().now()));
            assert!(matches!(
                completion.complete_final_async(&expired, final_params()).await,
                Outcome::Err(_)
            ));
            assert!(calls.lock().unwrap().is_empty());
            let mut params = final_params();
            params.argument.value = "cancel".into();
            assert!(matches!(
                completion.complete_final_async(&ctx, params).await,
                Outcome::Err(_)
            ));
            assert_eq!(calls.lock().unwrap().len(), 1);
            let peer = McpContext::new(ctx.cx().clone(), 97);
            let drain = peer.clone().with_operation_deadline(Some(
                peer.cx().now().saturating_add_nanos(5_000_000_000),
            ));
            lane.wait_idle(&drain).await.unwrap();
            assert!(matches!(
                completion.complete_final_async(&peer, final_params()).await,
                Outcome::Ok(_)
            ));
            assert_eq!(calls.lock().unwrap().len(), 2);
        });
    }

    #[test]
    fn abandoned_prompt_keeps_the_shared_resource_and_completion_lane_charged() {
        struct HeldPrompt {
            started: Mutex<Option<oneshot::Sender<()>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
        }
        impl PromptHandler for HeldPrompt {
            fn definition(&self) -> Prompt {
                Prompt {
                    name: "held".into(),
                    description: None,
                    arguments: vec![],
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn get(
                &self,
                _ctx: &McpContext,
                _arguments: HashMap<String, String>,
            ) -> McpResult<Vec<PromptMessage>> {
                self.started
                    .lock()
                    .unwrap()
                    .take()
                    .unwrap()
                    .send_blocking(())
                    .unwrap();
                self.release
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap();
                Ok(Vec::new())
            }
        }
        struct ReadyResource(Arc<AtomicUsize>);
        impl ResourceHandler for ReadyResource {
            fn definition(&self) -> Resource {
                Resource {
                    uri: "report://ready".into(),
                    name: "Ready".into(),
                    description: None,
                    mime_type: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                }
            }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(vec![
                    serde_json::from_value(json!({"uri": "report://ready", "text": "ready"})).unwrap(),
                ])
            }
        }
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 97);
            let peer = McpContext::new(ctx.cx().clone(), 97);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let (started, mut entered) = oneshot::channel();
            let (release, blocked) = std::sync::mpsc::sync_channel(1);
            let prompt = BlockingPrompt::from_sync_hooks(
                HeldPrompt {
                    started: Mutex::new(Some(started)),
                    release: Mutex::new(blocked),
                },
                lane.clone(),
            )
            .unwrap();
            let observed = Arc::new(AtomicUsize::new(0));
            let resource = BlockingResource::from_sync_hooks(
                ReadyResource(Arc::clone(&observed)),
                lane.clone(),
            )
            .unwrap();
            let (completion, calls) = fixture(lane.clone());
            let mut call = prompt.get_async(&ctx, HashMap::new());
            poll_fn(|task| {
                assert!(Future::poll(call.as_mut(), task).is_pending());
                Poll::Ready(())
            })
            .await;
            entered.recv(ctx.cx()).await.unwrap();
            drop(call);
            assert_eq!(
                lane.in_flight().unwrap(),
                1,
                "dropping the waiter is not worker completion"
            );
            assert!(matches!(resource.read_async(&peer).await, Outcome::Err(_)));
            assert!(matches!(
                completion.complete_final_async(&peer, final_params()).await,
                Outcome::Err(_)
            ));
            assert_eq!(observed.load(Ordering::SeqCst), 0);
            assert!(calls.lock().unwrap().is_empty());
            assert!(peer.ensure_live().is_ok());
            release.send(()).unwrap();
            let drain = peer.clone().with_operation_deadline(Some(
                peer.cx().now().saturating_add_nanos(5_000_000_000),
            ));
            lane.wait_idle(&drain).await.unwrap();
            let Outcome::Ok(contents) = resource.read_async(&peer).await else {
                panic!("resource must recover capacity")
            };
            assert_eq!(serde_json::to_value(contents).unwrap()[0]["text"], "ready");
            assert!(matches!(
                completion.complete_final_async(&peer, final_params()).await,
                Outcome::Ok(_)
            ));
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            assert_eq!(calls.lock().unwrap().len(), 1);
            lane.close().unwrap();
            assert!(matches!(
                completion.complete_legacy_async(&peer, legacy_params()).await,
                Outcome::Err(_)
            ));
            assert_eq!(calls.lock().unwrap().len(), 1);
        });
    }
}
