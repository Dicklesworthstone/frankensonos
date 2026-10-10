//! Caller-pool execution of explicitly selected synchronous resource hooks.

use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult, Outcome};
use fastmcp_protocol::common_types::{Annotations, OpenMetadata, RawIcon};
use fastmcp_protocol::{
    CompleteResult, FinalReadResourceResult, FinalResource, FinalResourceTemplate, Icon,
    Resource, ResourceContent, ResourceTemplate,
};

use super::{BlockingHandlerLane, outcome};
use crate::bidirectional::MrtrCompletedInputs;
use crate::handler::{
    BoxFuture, FinalMethodOutcome, FinalResourceReadCacheHintProvenance, ResourceHandler, UriParams,
};

/// Runs synchronous resource reads and subscription hooks on the caller's pool.
///
/// Register this through `ServerBuilder::resource`. The original concrete or
/// template catalog, URI policy, timeout and cache-hint provenance remain the
/// router's inputs. Each async entry point selects its corresponding synchronous
/// hook exactly once; URI-taking hooks receive owned copies of the exact URI and
/// capture map, including an empty map. No URI normalization is performed.
///
/// Unlike tools, `ResourceHandler` has no execution-mode declaration. Therefore
/// construction explicitly selects **sync hooks**, not an automatic conversion
/// of an arbitrary async handler. Do not wrap handlers whose behavior lives in
/// async overrides, an upstream proxy, or another blocking adapter. Declared MRTR
/// handlers are refused because there is no synchronous resumption hook.
///
/// There is no inline or private-pool fallback. Registration hooks and handler
/// destructors must remain nonblocking. A running syscall cannot be preempted;
/// its shared-lane reservation lasts until actual worker/result custody ends.
pub struct BlockingResource<H> {
    handler: Arc<H>,
    lane: BlockingHandlerLane,
}

impl<H: ResourceHandler + 'static> BlockingResource<H> {
    /// Explicitly chooses this handler's synchronous execution hooks.
    /// Construction performs no I/O and does not create an executor.
    pub fn from_sync_hooks(handler: H, lane: BlockingHandlerLane) -> McpResult<Self> {
        if handler.declares_final_mrtr() {
            return Err(no_resume());
        }
        Ok(Self { handler: Arc::new(handler), lane })
    }

    fn execute<'a, T, F>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, work: F,
    ) -> BoxFuture<'a, McpOutcome<T>>
    where
        T: Send + 'static,
        F: FnOnce(&H, &McpContext) -> McpResult<T> + Send + 'static,
    {
        let handler = Arc::clone(&self.handler);
        Box::pin(async move {
            outcome(self.lane.execute(ctx, cx, move |ctx| work(handler.as_ref(), ctx)).await)
        })
    }
}

fn async_required() -> McpError {
    McpError::invalid_request("blocking resource requires asynchronous caller-owned dispatch")
}

fn no_resume() -> McpError {
    McpError::invalid_request("blocking resource has no synchronous resume hook")
}

fn complete_only(
    result: FinalMethodOutcome<FinalReadResourceResult>,
) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
    match result {
        FinalMethodOutcome::Complete(_) => Ok(result),
        FinalMethodOutcome::InputRequired(_) => Err(no_resume()),
    }
}

impl<H: ResourceHandler + 'static> ResourceHandler for BlockingResource<H> {
    fn definition(&self) -> Resource { self.handler.definition() }
    fn template(&self) -> Option<ResourceTemplate> { self.handler.template() }
    fn final_definition(&self) -> Option<FinalResource> { self.handler.final_definition() }
    fn final_template_definition(&self) -> Option<FinalResourceTemplate> {
        self.handler.final_template_definition()
    }
    fn final_client_direct_https(&self) -> bool { self.handler.final_client_direct_https() }
    fn final_title(&self) -> Option<&str> { self.handler.final_title() }
    fn final_icons(&self) -> Option<&[RawIcon]> { self.handler.final_icons() }
    fn final_annotations(&self) -> Option<&Annotations> { self.handler.final_annotations() }
    fn final_metadata(&self) -> Option<&OpenMetadata> { self.handler.final_metadata() }
    fn final_template_title(&self) -> Option<&str> { self.handler.final_template_title() }
    fn final_template_icons(&self) -> Option<&[RawIcon]> { self.handler.final_template_icons() }
    fn final_template_annotations(&self) -> Option<&Annotations> {
        self.handler.final_template_annotations()
    }
    fn final_template_metadata(&self) -> Option<&OpenMetadata> {
        self.handler.final_template_metadata()
    }
    fn final_resource_read_cache_hint_provenance(&self) -> FinalResourceReadCacheHintProvenance {
        self.handler.final_resource_read_cache_hint_provenance()
    }
    fn icon(&self) -> Option<&Icon> { self.handler.icon() }
    fn version(&self) -> Option<&str> { self.handler.version() }
    fn tags(&self) -> &[String] { self.handler.tags() }
    fn timeout(&self) -> Option<Duration> { self.handler.timeout() }

    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(async_required())
    }
    fn read_with_uri(
        &self, _ctx: &McpContext, _uri: &str, _params: &UriParams,
    ) -> McpResult<Vec<ResourceContent>> {
        Err(async_required())
    }
    fn on_subscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(async_required())
    }
    fn on_unsubscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(async_required())
    }
    fn on_subscribe_async<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str,
    ) -> BoxFuture<'a, McpResult<()>> {
        let handler = Arc::clone(&self.handler);
        let uri = uri.to_owned();
        Box::pin(async move {
            self.lane.execute(ctx, ctx.cx(), move |ctx| handler.on_subscribe(ctx, &uri)).await
        })
    }
    fn on_unsubscribe_async<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str,
    ) -> BoxFuture<'a, McpResult<()>> {
        let handler = Arc::clone(&self.handler);
        let uri = uri.to_owned();
        Box::pin(async move {
            self.lane.execute(ctx, ctx.cx(), move |ctx| handler.on_unsubscribe(ctx, &uri)).await
        })
    }
    fn read_async<'a>(
        &'a self, ctx: &'a McpContext,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        self.execute(ctx, ctx.cx(), |handler, ctx| handler.read(ctx))
    }
    fn read_async_with_uri<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        self.read_async_with_uri_in_request(ctx, ctx.cx(), uri, params)
    }
    fn read_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<Vec<ResourceContent>>> {
        let (uri, params) = (uri.to_owned(), params.clone());
        self.execute(ctx, cx, move |handler, ctx| handler.read_with_uri(ctx, &uri, &params))
    }
    fn read_final_async<'a>(
        &'a self, ctx: &'a McpContext,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        self.execute(ctx, ctx.cx(), |handler, ctx| handler.read_final(ctx))
    }
    fn read_final_async_with_uri<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        self.read_final_async_with_uri_in_request(ctx, ctx.cx(), uri, params)
    }
    fn read_final_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        let (uri, params) = (uri.to_owned(), params.clone());
        self.execute(ctx, cx, move |handler, ctx| handler.read_final_with_uri(ctx, &uri, &params))
    }
    fn read_final_outcome_async<'a>(
        &'a self, ctx: &'a McpContext,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        self.execute(ctx, ctx.cx(), |handler, ctx| complete_only(handler.read_final_outcome(ctx)?))
    }
    fn read_final_outcome_async_with_uri<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        self.read_final_outcome_async_with_uri_in_request(ctx, ctx.cx(), uri, params)
    }
    fn read_final_outcome_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        let (uri, params) = (uri.to_owned(), params.clone());
        self.execute(ctx, cx, move |handler, ctx| {
            complete_only(handler.read_final_outcome_with_uri(ctx, &uri, &params)?)
        })
    }
    fn read_final_outcome_async_with_uri_resuming_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, params: &'a UriParams,
        resume: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        if resume.is_some() {
            return Box::pin(async { Outcome::Err(no_resume()) });
        }
        self.read_final_outcome_async_with_uri_in_request(ctx, cx, uri, params)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{Future, poll_fn};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::Poll;
    use asupersync::channel::oneshot;
    use fastmcp_protocol::{
        ClientCapabilities, CoreRequest, CoreResult, FinalCoreResult, FinalRequestMeta,
    };
    use fastmcp_protocol::protocol_policy::ProtocolEra;
    use serde_json::json;

    type Calls = Arc<Mutex<Vec<(&'static str, String, UriParams)>>>;
    struct Document { calls: Calls, poller: std::thread::ThreadId, resuming: bool }
    impl Document {
        fn record(&self, ctx: &McpContext, hook: &'static str, uri: &str, params: &UriParams) {
            assert_ne!(std::thread::current().id(), self.poller);
            assert_eq!(ctx.request_id(), 73);
            assert_eq!(Cx::current().unwrap().task_id(), ctx.task_id());
            self.calls.lock().unwrap().push((hook, uri.to_owned(), params.clone()));
        }
    }
    fn definition() -> Resource {
        Resource { uri: "report://monthly/alpha".into(), name: "Report".into(),
            description: None, mime_type: Some("text/plain".into()),
            icon: None, version: None, tags: vec!["report".into()] }
    }
    fn content(uri: &str) -> Vec<ResourceContent> {
        vec![serde_json::from_value(json!({"uri":uri,"text":"resource-value"})).unwrap()]
    }
    fn final_result() -> CompleteResult<FinalReadResourceResult> {
        let request = CoreRequest::decode(ProtocolEra::Modern2026, "resources/read", Some(&json!({
            "uri":"report://monthly/alpha", "_meta":FinalRequestMeta::new(ClientCapabilities::default())
        }))).unwrap();
        let CoreResult::Final(FinalCoreResult::ResourcesRead { result, .. }) = request.decode_result(r#"{
            "resultType":"complete","contents":[{"uri":"report://monthly/alpha","text":"final-value"}],
            "ttlMs":731,"cacheScope":"private","_meta":{"com.example/revision":7},
            "x-exact":{"z":900719925474099312345,"a":1.20e+4}
        }"#).unwrap() else { panic!("expected exact final resource result") };
        result
    }
    fn encode(result: CompleteResult<FinalReadResourceResult>) -> String {
        CoreResult::Final(FinalCoreResult::ResourcesRead { result, diagnostic: None }).encode().unwrap()
    }
    impl ResourceHandler for Document {
        fn definition(&self) -> Resource { definition() }
        fn declares_final_mrtr(&self) -> bool { self.resuming }
        fn final_definition(&self) -> Option<FinalResource> {
            Some(serde_json::from_value(json!({"uri":"report://monthly/alpha","name":"Report",
                "title":"Exact title","size":17,"_meta":{"com.example/catalog":true}})).unwrap())
        }
        fn final_resource_read_cache_hint_provenance(&self) -> FinalResourceReadCacheHintProvenance {
            FinalResourceReadCacheHintProvenance::Explicit
        }
        fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
            self.record(ctx, "read", "", &UriParams::new());
            Ok(content("report://monthly/alpha"))
        }
        fn read_with_uri(&self, ctx: &McpContext, uri: &str, params: &UriParams) -> McpResult<Vec<ResourceContent>> {
            self.record(ctx, "read_uri", uri, params);
            Ok(content(uri))
        }
        fn read_final(&self, ctx: &McpContext) -> McpResult<CompleteResult<FinalReadResourceResult>> {
            self.record(ctx, "final", "", &UriParams::new());
            Ok(final_result())
        }
        fn read_final_with_uri(&self, ctx: &McpContext, uri: &str, params: &UriParams)
            -> McpResult<CompleteResult<FinalReadResourceResult>>
        {
            self.record(ctx, "final_uri", uri, params);
            Ok(final_result())
        }
        fn read_final_outcome(&self, ctx: &McpContext) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
            self.read_final(ctx).map(FinalMethodOutcome::Complete)
        }
        fn read_final_outcome_with_uri(&self, ctx: &McpContext, uri: &str, params: &UriParams)
            -> McpResult<FinalMethodOutcome<FinalReadResourceResult>>
        {
            self.read_final_with_uri(ctx, uri, params).map(FinalMethodOutcome::Complete)
        }
        fn on_subscribe(&self, ctx: &McpContext, uri: &str) -> McpResult<()> {
            self.record(ctx, "subscribe", uri, &UriParams::new()); Ok(())
        }
        fn on_unsubscribe(&self, ctx: &McpContext, uri: &str) -> McpResult<()> {
            self.record(ctx, "unsubscribe", uri, &UriParams::new()); Ok(())
        }
    }
    fn runtime(pool: bool) -> asupersync::runtime::Runtime {
        let builder = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap());
        if pool { builder.blocking_threads(0, 2).build().unwrap() } else { builder.build().unwrap() }
    }
    fn fixture(lane: BlockingHandlerLane) -> (BlockingResource<Document>, Calls) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = Document { calls: Arc::clone(&calls), poller: std::thread::current().id(), resuming: false };
        (BlockingResource::from_sync_hooks(handler, lane).unwrap(), calls)
    }

    #[test]
    fn resource_sync_paths_refuse_without_work_and_async_legacy_hooks_run_on_pool() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let (resource, calls) = fixture(lane.clone());
            let resource: &dyn ResourceHandler = &resource;
            let uri = "report://monthly/a%252Fb";
            let params = UriParams::from([("name".into(), "a%2Fb".into())]);
            let before = params.clone();
            assert!(resource.read(&ctx).is_err());
            assert!(resource.read_with_uri(&ctx, uri, &params).is_err());
            assert!(resource.read_final(&ctx).is_err());
            assert!(resource.on_subscribe(&ctx, uri).is_err());
            assert!(resource.on_unsubscribe(&ctx, uri).is_err());
            assert!(calls.lock().unwrap().is_empty());
            assert!(matches!(resource.read_async(&ctx).await, Outcome::Ok(_)));
            let Outcome::Ok(contents) = resource.read_async_with_uri(&ctx, uri, &params).await
                else { panic!("URI read must complete") };
            assert_eq!(serde_json::to_value(contents).unwrap()[0]["uri"], uri);
            assert!(matches!(resource.read_async_with_uri_in_request(&ctx, ctx.cx(), uri, &params).await, Outcome::Ok(_)));
            assert_eq!(params, before);
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 3);
            assert_eq!(calls[0].0, "read");
            assert_eq!(calls[1], ("read_uri", uri.to_owned(), params.clone()));
            assert_eq!(calls[2], calls[1]);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }

    #[test]
    fn final_resource_hooks_preserve_full_envelope_and_empty_uri_capture_maps() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let (resource, calls) = fixture(BlockingHandlerLane::new(1).unwrap());
            let expected = encode(final_result());
            let empty = UriParams::new();
            let uri = "report://monthly/alpha";
            for result in [
                resource.read_final_async(&ctx).await,
                resource.read_final_async_with_uri(&ctx, uri, &empty).await,
                resource.read_final_async_with_uri_in_request(&ctx, ctx.cx(), uri, &empty).await,
            ] {
                let Outcome::Ok(result) = result else { panic!("complete result expected") };
                assert_eq!(encode(result), expected);
            }
            for result in [
                resource.read_final_outcome_async(&ctx).await,
                resource.read_final_outcome_async_with_uri(&ctx, uri, &empty).await,
                resource.read_final_outcome_async_with_uri_in_request(&ctx, ctx.cx(), uri, &empty).await,
                resource.read_final_outcome_async_with_uri_resuming_in_request(&ctx, ctx.cx(), uri, &empty, None).await,
            ] {
                let Outcome::Ok(FinalMethodOutcome::Complete(result)) = result
                    else { panic!("complete outcome expected") };
                assert_eq!(encode(result), expected);
            }
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 7);
            assert_eq!(calls.iter().filter(|(hook, _, _)| *hook == "final_uri").count(), 5);
            assert!(calls.iter().filter(|(hook, _, _)| *hook == "final_uri")
                .all(|(_, matched, captures)| matched == uri && captures.is_empty()));
        });
    }

    #[test]
    fn resource_catalog_and_cache_policy_are_not_replaced_by_the_adapter() {
        let lane = BlockingHandlerLane::new(1).unwrap();
        let (resource, calls) = fixture(lane);
        let actual = serde_json::to_value(resource.final_definition().unwrap()).unwrap();
        assert_eq!(actual["title"], "Exact title");
        assert_eq!(actual["size"], 17);
        assert_eq!(actual["_meta"]["com.example/catalog"], true);
        assert_eq!(resource.definition().uri, definition().uri);
        assert_eq!(resource.final_resource_read_cache_hint_provenance(), FinalResourceReadCacheHintProvenance::Explicit);
        assert!(!resource.declares_final_mrtr());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn resource_template_catalog_and_uri_hooks_survive_offload() {
        struct Template(Document, FinalResourceTemplate);
        impl ResourceHandler for Template {
            fn definition(&self) -> Resource {
                Resource { uri: self.1.uri_template.clone(), ..definition() }
            }
            fn template(&self) -> Option<ResourceTemplate> {
                Some(ResourceTemplate { uri_template: self.1.uri_template.clone(),
                    name: self.1.name.clone(), description: self.1.description.clone(),
                    mime_type: self.1.mime_type.clone(), icon: None, version: None, tags: vec![] })
            }
            fn final_template_definition(&self) -> Option<FinalResourceTemplate> { Some(self.1.clone()) }
            fn final_template_metadata(&self) -> Option<&OpenMetadata> { self.1.meta.as_ref() }
            fn read(&self, ctx: &McpContext) -> McpResult<Vec<ResourceContent>> { self.0.read(ctx) }
            fn read_with_uri(&self, ctx: &McpContext, uri: &str, params: &UriParams) -> McpResult<Vec<ResourceContent>> {
                self.0.read_with_uri(ctx, uri, params)
            }
        }
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let calls = Arc::new(Mutex::new(Vec::new()));
            let definition: FinalResourceTemplate = serde_json::from_value(json!({
                "uriTemplate":"report://monthly/{name}","name":"Reports","title":"Full template",
                "mimeType":"text/plain","annotations":{"priority":0.5},
                "_meta":{"com.example/template":17}
            })).unwrap();
            let resource = BlockingResource::from_sync_hooks(Template(Document {
                calls: Arc::clone(&calls), poller: std::thread::current().id(), resuming: false,
            }, definition.clone()), BlockingHandlerLane::new(1).unwrap()).unwrap();
            assert_eq!(serde_json::to_value(resource.final_template_definition().unwrap()).unwrap(),
                serde_json::to_value(definition).unwrap());
            assert_eq!(resource.template().unwrap().uri_template, "report://monthly/{name}");
            assert!(resource.final_template_metadata().is_some());
            assert!(resource.final_definition().is_none());
            let params = UriParams::from([("name".into(), "alpha".into())]);
            let Outcome::Ok(contents) = resource.read_async_with_uri_in_request(
                &ctx, ctx.cx(), "report://monthly/alpha", &params,
            ).await else { panic!("template read must complete") };
            assert_eq!(serde_json::to_value(contents).unwrap()[0]["text"], "resource-value");
            assert_eq!(calls.lock().unwrap().as_slice(), &[("read_uri", "report://monthly/alpha".into(), params)]);
        });
    }

    #[test]
    fn subscription_hooks_use_the_same_caller_pool_without_inline_fallback() {
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let (resource, calls) = fixture(BlockingHandlerLane::new(1).unwrap());
            let uri = "report://monthly/a%252Fb";
            resource.on_subscribe_async(&ctx, uri).await.unwrap();
            resource.on_unsubscribe_async(&ctx, uri).await.unwrap();
            let calls = calls.lock().unwrap();
            assert_eq!(calls.len(), 2);
            assert_eq!(calls[0], ("subscribe", uri.to_owned(), UriParams::new()));
            assert_eq!(calls[1], ("unsubscribe", uri.to_owned(), UriParams::new()));
        });
    }

    #[test]
    fn missing_pool_closed_lane_and_precancellation_refuse_without_resource_effects() {
        for case in 0..3 {
            runtime(case != 0).block_on(async {
                let ctx = McpContext::new(Cx::current().unwrap(), 73);
                let lane = BlockingHandlerLane::new(1).unwrap();
                if case == 1 { lane.close().unwrap(); }
                if case == 2 { ctx.request_cancellation().cancel(); }
                let (resource, calls) = fixture(lane.clone());
                assert!(matches!(resource.read_async(&ctx).await, Outcome::Err(_)));
                assert!(resource.on_subscribe_async(&ctx, "report://monthly/alpha").await.is_err());
                assert!(calls.lock().unwrap().is_empty());
                assert_eq!(lane.in_flight().unwrap(), 0);
            });
        }
    }

    #[test]
    fn cancelling_a_blocked_resource_keeps_capacity_and_does_not_cancel_its_peer() {
        struct Held {
            started: Mutex<Option<oneshot::Sender<()>>>,
            release: Mutex<std::sync::mpsc::Receiver<()>>,
            calls: Arc<AtomicUsize>,
        }
        impl ResourceHandler for Held {
            fn definition(&self) -> Resource { definition() }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.lock().unwrap().take().unwrap().send_blocking(()).unwrap();
                self.release.lock().unwrap().recv_timeout(Duration::from_secs(5)).unwrap();
                Ok(content("report://monthly/alpha"))
            }
        }
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let peer = McpContext::new(ctx.cx().clone(), 73);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let (started, mut entered) = oneshot::channel();
            let (release, blocked) = std::sync::mpsc::sync_channel(1);
            let observed = Arc::new(AtomicUsize::new(0));
            let resource = BlockingResource::from_sync_hooks(Held {
                started: Mutex::new(Some(started)), release: Mutex::new(blocked), calls: Arc::clone(&observed),
            }, lane.clone()).unwrap();
            let (peer_resource, peer_calls) = fixture(lane.clone());
            let mut call = resource.read_async(&ctx);
            poll_fn(|task| { assert!(Future::poll(call.as_mut(), task).is_pending()); Poll::Ready(()) }).await;
            entered.recv(ctx.cx()).await.unwrap();
            ctx.request_cancellation().cancel();
            assert!(matches!(call.await, Outcome::Err(_)));
            assert_eq!(lane.in_flight().unwrap(), 1);
            assert!(matches!(peer_resource.read_async(&peer).await, Outcome::Err(_)));
            assert!(peer.ensure_live().is_ok());
            assert!(peer_calls.lock().unwrap().is_empty());
            release.send(()).unwrap();
            let drain = peer.clone().with_operation_deadline(Some(peer.cx().now().saturating_add_nanos(5_000_000_000)));
            lane.wait_idle(&drain).await.unwrap();
            assert!(matches!(peer_resource.read_async(&peer).await, Outcome::Ok(_)));
            assert_eq!(observed.load(Ordering::SeqCst), 1);
            assert_eq!(peer_calls.lock().unwrap().len(), 1);
        });
    }

    #[test]
    fn resuming_resource_declarations_cannot_be_silently_downgraded_to_sync_reads() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let handler = Document { calls: Arc::clone(&calls), poller: std::thread::current().id(), resuming: true };
        assert!(BlockingResource::from_sync_hooks(handler, BlockingHandlerLane::new(1).unwrap()).is_err());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn undeclared_input_required_is_refused_without_retrying_the_resource() {
        struct NeedsInput(Arc<AtomicUsize>);
        impl ResourceHandler for NeedsInput {
            fn definition(&self) -> Resource { definition() }
            fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
                panic!("a final outcome must not be replaced by a legacy read")
            }
            fn read_final_outcome(&self, _ctx: &McpContext) -> McpResult<FinalMethodOutcome<FinalReadResourceResult>> {
                self.0.fetch_add(1, Ordering::SeqCst);
                let request = CoreRequest::decode(ProtocolEra::Modern2026, "resources/read", Some(&json!({
                    "uri":"report://monthly/alpha", "_meta":FinalRequestMeta::new(ClientCapabilities::default())
                }))).unwrap();
                let CoreResult::Final(FinalCoreResult::ResourcesReadInputRequired { result, .. }) = request
                    .decode_result(r#"{"resultType":"input_required","requestState":"opaque"}"#).unwrap()
                    else { panic!("input-required fixture") };
                Ok(FinalMethodOutcome::InputRequired(result))
            }
        }
        runtime(true).block_on(async {
            let ctx = McpContext::new(Cx::current().unwrap(), 73);
            let calls = Arc::new(AtomicUsize::new(0));
            let lane = BlockingHandlerLane::new(1).unwrap();
            let resource = BlockingResource::from_sync_hooks(NeedsInput(Arc::clone(&calls)), lane.clone()).unwrap();
            assert!(matches!(resource.read_final_outcome_async(&ctx).await, Outcome::Err(_)));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }
}
