//! Caller-owned blocking execution for synchronous prompt rendering.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult, Outcome};
use fastmcp_protocol::common_types::{OpenMetadata, RawIcon};
use fastmcp_protocol::{
    CompleteResult, FinalGetPromptResult, FinalPrompt, Icon, Prompt, PromptMessage,
};

use super::{BlockingHandlerLane, outcome};
use crate::bidirectional::MrtrCompletedInputs;
use crate::handler::{BoxFuture, FinalMethodOutcome, PromptHandler};

/// Offloads the explicitly selected synchronous hooks of a prompt handler.
///
/// This is for renderers that perform synchronous database, filesystem or CPU
/// work. Register through `ServerBuilder::prompt`; share a `BlockingHandlerLane`
/// with tools, resources and completions to enforce one aggregate work ceiling.
/// Final results and catalog metadata are not projected through legacy types.
///
/// `PromptHandler` has no execution-mode marker, so `from_sync_hooks` is an
/// explicit choice of `get`, `get_final` and `get_final_outcome`, not detection
/// of async overrides. Do not wrap an async-only handler, proxy or another
/// blocking adapter. MRTR declarations and input-required outcomes are refused
/// rather than dropping their resumption semantics. Synchronous entry points
/// never execute user work inline. Registration hooks/destructors must not block.
pub struct BlockingPrompt<H> {
    handler: Arc<H>,
    lane: BlockingHandlerLane,
}

impl<H: PromptHandler + 'static> BlockingPrompt<H> {
    /// Selects synchronous hooks without installing a runtime or a worker pool.
    pub fn from_sync_hooks(handler: H, lane: BlockingHandlerLane) -> McpResult<Self> {
        if handler.declares_final_mrtr() {
            return Err(no_resume());
        }
        Ok(Self {
            handler: Arc::new(handler),
            lane,
        })
    }

    fn legacy<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<Vec<PromptMessage>>> {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(
                self.lane
                    .execute(ctx, cx, move |ctx| handler.get(ctx, arguments))
                    .await,
            )
        })
    }

    fn complete<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalGetPromptResult>>> {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(
                self.lane
                    .execute(ctx, cx, move |ctx| handler.get_final(ctx, arguments))
                    .await,
            )
        })
    }

    fn final_outcome<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(
                self.lane
                    .execute(ctx, cx, move |ctx| {
                        let result = handler.get_final_outcome(ctx, arguments)?;
                        match result {
                            FinalMethodOutcome::Complete(_) => Ok(result),
                            FinalMethodOutcome::InputRequired(_) => Err(no_resume()),
                        }
                    })
                    .await,
            )
        })
    }
}

fn no_resume() -> McpError {
    McpError::invalid_request("blocking prompt has no synchronous resume hook")
}

impl<H: PromptHandler + 'static> PromptHandler for BlockingPrompt<H> {
    fn definition(&self) -> Prompt {
        self.handler.definition()
    }
    fn final_definition(&self) -> Option<FinalPrompt> {
        self.handler.final_definition()
    }
    fn final_client_direct_https(&self) -> bool {
        self.handler.final_client_direct_https()
    }
    fn final_title(&self) -> Option<&str> {
        self.handler.final_title()
    }
    fn final_icons(&self) -> Option<&[RawIcon]> {
        self.handler.final_icons()
    }
    fn final_metadata(&self) -> Option<&OpenMetadata> {
        self.handler.final_metadata()
    }
    fn icon(&self) -> Option<&Icon> {
        self.handler.icon()
    }
    fn version(&self) -> Option<&str> {
        self.handler.version()
    }
    fn tags(&self) -> &[String] {
        self.handler.tags()
    }
    fn timeout(&self) -> Option<Duration> {
        self.handler.timeout()
    }

    fn get(
        &self,
        _ctx: &McpContext,
        _arguments: HashMap<String, String>,
    ) -> McpResult<Vec<PromptMessage>> {
        Err(McpError::invalid_request(
            "blocking prompt requires asynchronous caller-owned dispatch",
        ))
    }
    fn get_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<Vec<PromptMessage>>> {
        self.legacy(ctx, ctx.cx(), arguments)
    }
    fn get_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<Vec<PromptMessage>>> {
        self.legacy(ctx, cx, arguments)
    }
    fn get_final_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalGetPromptResult>>> {
        self.complete(ctx, ctx.cx(), arguments)
    }
    fn get_final_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalGetPromptResult>>> {
        self.complete(ctx, cx, arguments)
    }
    fn get_final_outcome_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
        self.final_outcome(ctx, ctx.cx(), arguments)
    }
    fn get_final_outcome_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
        self.final_outcome(ctx, cx, arguments)
    }
    fn get_final_outcome_async_resuming_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        cx: &'a Cx,
        arguments: HashMap<String, String>,
        resume: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalGetPromptResult>>> {
        if resume.is_some() {
            return Box::pin(async { Outcome::Err(no_resume()) });
        }
        self.final_outcome(ctx, cx, arguments)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmcp_protocol::protocol_policy::ProtocolEra;
    use fastmcp_protocol::{
        ClientCapabilities, CoreRequest, CoreResult, FinalCoreResult, FinalRequestMeta,
    };
    use serde_json::json;
    use std::sync::Mutex;

    type Calls = Arc<Mutex<Vec<(&'static str, HashMap<String, String>)>>>;
    struct Renderer {
        calls: Calls,
        poller: std::thread::ThreadId,
        resuming: bool,
        asks_input: bool,
    }
    fn definition() -> Prompt {
        Prompt {
            name: "render".into(),
            description: Some("Renderer".into()),
            arguments: vec![],
            icon: None,
            version: None,
            tags: vec![],
        }
    }
    fn request() -> CoreRequest {
        CoreRequest::decode(
            ProtocolEra::Modern2026,
            "prompts/get",
            Some(&json!({
                "name":"render", "_meta":FinalRequestMeta::new(ClientCapabilities::default())
            })),
        )
        .unwrap()
    }
    fn final_result() -> CompleteResult<FinalGetPromptResult> {
        let CoreResult::Final(FinalCoreResult::PromptsGet { result, .. }) = request().decode_result(r#"{
            "resultType":"complete","description":"Exact rendering",
            "messages":[{"role":"user","content":{"type":"text","text":"final-rendered","_meta":{"com.example/block":true}}}],
            "_meta":{"com.example/result":11},"x-exact":{"z":900719925474099312345,"a":1.20e+4}
        }"#).unwrap() else { panic!("final prompt fixture") };
        result
    }
    fn encode(result: CompleteResult<FinalGetPromptResult>) -> String {
        CoreResult::Final(FinalCoreResult::PromptsGet {
            result,
            diagnostic: None,
        })
        .encode()
        .unwrap()
    }
    impl Renderer {
        fn record(&self, ctx: &McpContext, hook: &'static str, arguments: HashMap<String, String>) {
            assert_ne!(std::thread::current().id(), self.poller);
            assert_eq!(ctx.request_id(), 83);
            assert_eq!(Cx::current().unwrap().task_id(), ctx.task_id());
            self.calls.lock().unwrap().push((hook, arguments));
        }
    }
    impl PromptHandler for Renderer {
        fn definition(&self) -> Prompt {
            definition()
        }
        fn declares_final_mrtr(&self) -> bool {
            self.resuming
        }
        fn final_client_direct_https(&self) -> bool {
            true
        }
        fn timeout(&self) -> Option<Duration> {
            Some(Duration::from_secs(3))
        }
        fn final_definition(&self) -> Option<FinalPrompt> {
            Some(serde_json::from_value(json!({"name":"render","title":"Complete catalog",
                "arguments":[{"name":"body","title":"Body","required":true},{"name":"style","title":"Style"}],
                "_meta":{"com.example/prompt":true}})).unwrap())
        }
        fn get(
            &self,
            ctx: &McpContext,
            arguments: HashMap<String, String>,
        ) -> McpResult<Vec<PromptMessage>> {
            self.record(ctx, "legacy", arguments);
            Ok(vec![
                serde_json::from_value(
                    json!({"role":"user","content":{"type":"text","text":"legacy-rendered"}}),
                )
                .unwrap(),
            ])
        }
        fn get_final(
            &self,
            ctx: &McpContext,
            arguments: HashMap<String, String>,
        ) -> McpResult<CompleteResult<FinalGetPromptResult>> {
            self.record(ctx, "final", arguments);
            Ok(final_result())
        }
        fn get_final_outcome(
            &self,
            ctx: &McpContext,
            arguments: HashMap<String, String>,
        ) -> McpResult<FinalMethodOutcome<FinalGetPromptResult>> {
            if self.asks_input {
                self.record(ctx, "input", arguments);
                let CoreResult::Final(FinalCoreResult::PromptsGetInputRequired { result, .. }) =
                    request()
                        .decode_result(r#"{"resultType":"input_required","requestState":"opaque"}"#)
                        .unwrap()
                else {
                    panic!("input-required fixture")
                };
                return Ok(FinalMethodOutcome::InputRequired(result));
            }
            self.get_final(ctx, arguments)
                .map(FinalMethodOutcome::Complete)
        }
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
    fn renderer() -> (Renderer, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Renderer {
                calls: Arc::clone(&calls),
                poller: std::thread::current().id(),
                resuming: false,
                asks_input: false,
            },
            calls,
        )
    }
    fn arguments() -> HashMap<String, String> {
        HashMap::from([
            ("body".into(), "Unicode: 日本語\n{not metadata}".into()),
            ("_meta".into(), "ordinary data".into()),
        ])
    }

    #[test]
    fn prompt_legacy_hooks_run_once_on_pool_and_preserve_arguments() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 83);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let (handler, calls) = renderer();
            let prompt = BlockingPrompt::from_sync_hooks(handler, lane.clone()).unwrap();
            let prompt: &dyn PromptHandler = &prompt;
            assert!(prompt.get(&ctx, arguments()).is_err());
            assert!(prompt.get_final(&ctx, arguments()).is_err());
            assert!(prompt.get_final_outcome(&ctx, arguments()).is_err());
            assert!(calls.lock().unwrap().is_empty());
            for result in [
                prompt.get_async(&ctx, arguments()).await,
                prompt
                    .get_async_in_request(&ctx, ctx.cx(), arguments())
                    .await,
            ] {
                let Outcome::Ok(messages) = result else {
                    panic!("legacy prompt must render")
                };
                assert_eq!(
                    serde_json::to_value(messages).unwrap()[0]["content"]["text"],
                    "legacy-rendered"
                );
            }
            assert_eq!(
                calls.lock().unwrap().as_slice(),
                &[("legacy", arguments()), ("legacy", arguments())]
            );
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }

    #[test]
    fn every_final_prompt_hook_retains_exact_metadata_and_result_members() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 83);
            let (handler, calls) = renderer();
            let prompt =
                BlockingPrompt::from_sync_hooks(handler, BlockingHandlerLane::new(1).unwrap())
                    .unwrap();
            let expected = encode(final_result());
            for result in [
                prompt.get_final_async(&ctx, arguments()).await,
                prompt
                    .get_final_async_in_request(&ctx, ctx.cx(), arguments())
                    .await,
            ] {
                let Outcome::Ok(result) = result else {
                    panic!("final result expected")
                };
                assert_eq!(encode(result), expected);
            }
            for result in [
                prompt.get_final_outcome_async(&ctx, arguments()).await,
                prompt
                    .get_final_outcome_async_in_request(&ctx, ctx.cx(), arguments())
                    .await,
                prompt
                    .get_final_outcome_async_resuming_in_request(&ctx, ctx.cx(), arguments(), None)
                    .await,
            ] {
                let Outcome::Ok(FinalMethodOutcome::Complete(result)) = result else {
                    panic!("complete outcome expected")
                };
                assert_eq!(encode(result), expected);
            }
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 5);
            assert!(
                calls
                    .iter()
                    .all(|(hook, values)| *hook == "final" && *values == arguments())
            );
        });
    }

    #[test]
    fn prompt_registration_keeps_required_presence_title_timeout_and_uri_policy() {
        let (handler, calls) = renderer();
        let prompt =
            BlockingPrompt::from_sync_hooks(handler, BlockingHandlerLane::new(1).unwrap()).unwrap();
        let definition = serde_json::to_value(prompt.final_definition().unwrap()).unwrap();
        assert_eq!(definition["arguments"][0]["required"], true);
        assert!(definition["arguments"][1].get("required").is_none());
        assert_eq!(definition["arguments"][1]["title"], "Style");
        assert_eq!(definition["_meta"]["com.example/prompt"], true);
        assert!(prompt.final_client_direct_https());
        assert_eq!(prompt.timeout(), Some(Duration::from_secs(3)));
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn unavailable_pool_closed_lane_and_precancellation_never_render() {
        for case in 0..3 {
            runtime(case != 0).block_on(async {
                let ctx = McpContext::new(Cx::current().unwrap(), 83);
                let lane = BlockingHandlerLane::new(1).unwrap();
                if case == 1 {
                    lane.close().unwrap();
                }
                if case == 2 {
                    ctx.request_cancellation().cancel();
                }
                let (handler, calls) = renderer();
                let prompt = BlockingPrompt::from_sync_hooks(handler, lane.clone()).unwrap();
                assert!(matches!(
                    prompt.get_async(&ctx, arguments()).await,
                    Outcome::Err(_)
                ));
                assert!(matches!(
                    prompt.get_final_outcome_async(&ctx, arguments()).await,
                    Outcome::Err(_)
                ));
                assert!(calls.lock().unwrap().is_empty());
                assert_eq!(lane.in_flight().unwrap(), 0);
            });
        }
    }

    #[test]
    fn declared_and_undeclared_mrtr_are_not_replaced_by_legacy_rendering() {
        let (mut handler, calls) = renderer();
        handler.resuming = true;
        assert!(
            BlockingPrompt::from_sync_hooks(handler, BlockingHandlerLane::new(1).unwrap()).is_err()
        );
        assert!(calls.lock().unwrap().is_empty());
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 83);
            let (mut handler, calls) = renderer();
            handler.asks_input = true;
            let lane = BlockingHandlerLane::new(1).unwrap();
            let prompt = BlockingPrompt::from_sync_hooks(handler, lane.clone()).unwrap();
            assert!(matches!(
                prompt.get_final_outcome_async(&ctx, arguments()).await,
                Outcome::Err(_)
            ));
            assert_eq!(calls.lock().unwrap().as_slice(), &[("input", arguments())]);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }

    #[test]
    fn prompt_panic_is_redacted_and_the_same_lane_can_render_again() {
        struct Panics;
        impl PromptHandler for Panics {
            fn definition(&self) -> Prompt {
                definition()
            }
            fn get(
                &self,
                _ctx: &McpContext,
                _arguments: HashMap<String, String>,
            ) -> McpResult<Vec<PromptMessage>> {
                panic!("private-renderer-panic-canary")
            }
        }
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 83);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let prompt = BlockingPrompt::from_sync_hooks(Panics, lane.clone()).unwrap();
            let Outcome::Err(error) = prompt.get_async(&ctx, HashMap::new()).await else {
                panic!("panic must become an error")
            };
            assert!(!format!("{error:?}").contains("private-renderer-panic-canary"));
            let (handler, calls) = renderer();
            let healthy = BlockingPrompt::from_sync_hooks(handler, lane.clone()).unwrap();
            assert!(matches!(
                healthy.get_async(&ctx, arguments()).await,
                Outcome::Ok(_)
            ));
            assert_eq!(calls.lock().unwrap().len(), 1);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }
}
