//! Authenticated dynamic resources on the existing managed request transport.
//!
//! The upstream URI template is a route, not a URL to fetch. A read must match
//! its bounded reversible language before it can use the managed login. The
//! exact matched URI is sent to the MCP endpoint; captured parameters never
//! become HTTP headers or a different fetch target.

mod completion;
pub use completion::ManagedOAuthCompletion;
mod machine;
pub use machine::ClientCredentialsProvider;

use std::collections::HashSet;
use std::sync::Arc;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult};
use fastmcp_protocol::common_types::{AbsoluteUri, Annotations, OpenMetadata, RawIcon};
use fastmcp_protocol::{
    CompleteResult, CoreResult, FinalCoreResult, FinalReadResourceResult,
    FinalResourceTemplate, Resource, ResourceContent, ResourceTemplate,
    ReversibleResourceTemplate, UriTemplate,
};
use serde_json::json;

use super::{Forwarder, MODERN_ASYNC_ONLY, ManagedOAuthProvider, UNEXPECTED_RESULT, outcome};
use crate::handler::{
    BoxFuture, FinalMethodOutcome, FinalResourceReadCacheHintProvenance, FinalResourceUriUse,
    ResourceHandler, ResourceUriUsePolicy, UriParams,
};

impl ManagedOAuthProvider {
    /// Materializes the entire authenticated resource-template catalog.
    ///
    /// Every entry must compile for the same deterministic reverse routing
    /// used by the server. A duplicate or non-reversible template rejects the
    /// collection without returning partial handlers. Namespaces do not alter
    /// template URIs. Register the returned handlers with `builder.resource`.
    /// This installs neither subscription relay nor MRTR continuation state.
    pub async fn resource_templates(&self, cx: &Cx) -> McpResult<Vec<ManagedOAuthResourceTemplate>> {
        let pages = self.catalog(cx, "resources/templates/list").await?;
        let mut entries = Vec::new();
        for page in pages {
            let CoreResult::Final(FinalCoreResult::ResourceTemplatesList { result, .. }) = page else {
                return Err(McpError::invalid_request(UNEXPECTED_RESULT));
            };
            entries.extend(result.payload.resource_templates);
        }
        build_templates(Arc::clone(&self.forwarder), entries)
    }
}

/// A fully admitted authenticated upstream template and its immutable matcher.
///
/// Construction is private: the only public source is a bounded managed
/// catalog. Local registration delegates this service account's access to
/// matching resources; protect it with the gateway's own authorization policy.
pub struct ManagedOAuthResourceTemplate {
    forwarder: Arc<Forwarder>,
    definition: FinalResourceTemplate,
    matcher: ReversibleResourceTemplate,
}

impl ManagedOAuthResourceTemplate {
    /// Exact upstream template metadata retained for modern discovery.
    pub fn catalog_definition(&self) -> &FinalResourceTemplate {
        &self.definition
    }

    async fn invoke(
        &self, ctx: &McpContext, cx: &Cx, uri: &str,
    ) -> McpResult<CompleteResult<FinalReadResourceResult>> {
        ctx.checkpoint()?;
        super::check_cx(cx)?;
        // A variable in the scheme position can expand to HTTPS even though
        // the catalog template does not literally begin with it. Apply the
        // server's URI-use policy to the actual target before authenticated I/O.
        let target = AbsoluteUri::parse(uri).map_err(|_| route_error())?;
        if !ResourceUriUsePolicy::server_mediated().admits(&target, FinalResourceUriUse::ResourceReadTarget) {
            return Err(route_error());
        }
        if self.matcher.match_uri(uri).map_err(|_| route_error())?.is_none() {
            return Err(route_error());
        }
        // Do not reconstruct from caller-supplied UriParams, percent-decode
        // twice, or prepend the provider namespace. The matched wire identity
        // is the only resource selector authorized by this handler.
        match self.forwarder.execute(ctx, cx, "resources/read", json!({"uri": uri})).await? {
            FinalCoreResult::ResourcesRead { result, .. } => Ok(result),
            _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
        }
    }
}

fn route_error() -> McpError {
    McpError::invalid_params("Resource URI does not match the managed OAuth template")
}

fn build_templates(
    forwarder: Arc<Forwarder>, entries: Vec<FinalResourceTemplate>,
) -> McpResult<Vec<ManagedOAuthResourceTemplate>> {
    let mut uris = HashSet::new();
    let mut handlers = Vec::with_capacity(entries.len());
    for definition in entries {
        if !uris.insert(definition.uri_template.clone()) {
            return Err(McpError::invalid_request("Authenticated upstream catalog contains duplicate resource templates"));
        }
        if !ResourceUriUsePolicy::server_mediated().admits_template(&definition.uri_template) {
            return Err(McpError::invalid_request("Client-direct HTTPS templates cannot be mediated by this provider"));
        }
        let matcher = UriTemplate::parse(&definition.uri_template)
            .and_then(|template| template.compile_reversible())
            .map_err(|_| McpError::invalid_request("Authenticated upstream template is not reversibly routable"))?;
        handlers.push(ManagedOAuthResourceTemplate {
            forwarder: Arc::clone(&forwarder), definition, matcher,
        });
    }
    Ok(handlers)
}

impl ResourceHandler for ManagedOAuthResourceTemplate {
    // Registration fallback only; no lossy legacy execution is installed.
    fn definition(&self) -> Resource {
        Resource {
            uri: self.definition.uri_template.clone(),
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            mime_type: self.definition.mime_type.clone(),
            icon: None, version: None, tags: Vec::new(),
        }
    }
    fn template(&self) -> Option<ResourceTemplate> {
        Some(ResourceTemplate {
            uri_template: self.definition.uri_template.clone(),
            name: self.definition.name.clone(),
            description: self.definition.description.clone(),
            mime_type: self.definition.mime_type.clone(),
            icon: None, version: None, tags: Vec::new(),
        })
    }
    fn final_template_definition(&self) -> Option<FinalResourceTemplate> {
        Some(self.definition.clone())
    }
    fn final_template_title(&self) -> Option<&str> { self.definition.title.as_deref() }
    fn final_template_icons(&self) -> Option<&[RawIcon]> { self.definition.icons.as_deref() }
    fn final_template_annotations(&self) -> Option<&Annotations> { self.definition.annotations.as_ref() }
    fn final_template_metadata(&self) -> Option<&OpenMetadata> { self.definition.meta.as_ref() }
    fn final_resource_read_cache_hint_provenance(&self) -> FinalResourceReadCacheHintProvenance {
        FinalResourceReadCacheHintProvenance::Explicit
    }
    fn read(&self, _ctx: &McpContext) -> McpResult<Vec<ResourceContent>> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn on_subscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(McpError::invalid_request("Managed OAuth resource subscriptions are not installed"))
    }
    fn on_unsubscribe(&self, _ctx: &McpContext, _uri: &str) -> McpResult<()> {
        Err(McpError::invalid_request("Managed OAuth resource subscriptions are not installed"))
    }
    fn read_final_async_with_uri<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), uri).await) })
    }
    fn read_final_outcome_async_with_uri<'a>(
        &'a self, ctx: &'a McpContext, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), uri).await.map(FinalMethodOutcome::Complete)) })
    }
    fn read_final_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, uri).await) })
    }
    fn read_final_outcome_async_with_uri_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, uri: &'a str, _params: &'a UriParams,
    ) -> BoxFuture<'a, McpOutcome<FinalMethodOutcome<FinalReadResourceResult>>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, uri).await.map(FinalMethodOutcome::Complete)) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use fastmcp_core::block_on;
    use fastmcp_protocol::{CoreRequest, RequestId};
    use serde_json::Value;
    use super::super::{CoreBackend, core_request};
    use fastmcp_client::http_auth::rpc::ManagedCoreLimits;

    struct Backend {
        calls: Mutex<Vec<(RequestId, Value)>>,
        responses: Mutex<VecDeque<FinalCoreResult>>,
        cancel: bool,
    }
    impl CoreBackend for Backend {
        fn execute<'a>(
            &'a self, ctx: &'a McpContext, _cx: &'a Cx, request: CoreRequest,
            id: RequestId, _limits: ManagedCoreLimits,
        ) -> BoxFuture<'a, McpResult<FinalCoreResult>> {
            Box::pin(async move {
                self.calls.lock().unwrap().push((id, request.encode_params().unwrap().unwrap()));
                if self.cancel { ctx.request_cancellation().cancel(); }
                self.responses.lock().unwrap().pop_front()
                    .ok_or_else(|| McpError::internal_error("test response queue exhausted"))
            })
        }
    }
    fn definition(uri: &str) -> FinalResourceTemplate {
        serde_json::from_value(json!({
            "uriTemplate":uri,"name":"Report","title":"Monthly reports",
            "description":"A report by name","mimeType":"text/plain",
            "annotations":{"audience":["assistant"],"priority":0.5},
            "icons":[{"src":"https://example.com/report.png"}],
            "_meta":{"com.example/catalog":{"revision":17}}
        })).unwrap()
    }
    fn result() -> FinalCoreResult {
        let request = core_request("resources/read", json!({"uri":"report://monthly/alpha"}), None).unwrap();
        let CoreResult::Final(result) = request.decode_result(r#"{
            "resultType":"complete","contents":[
                {"uri":"report://monthly/alpha","text":"first"},
                {"uri":"report://attachments/alpha","blob":"AAEC"}
            ],"ttlMs":4321,"cacheScope":"private",
            "_meta":{"com.example/revision":17},
            "extra":{"z":900719925474099312345,"a":1.20e+4}
        }"#).unwrap() else { panic!("expected final result") };
        result
    }
    fn fixture(responses: Vec<FinalCoreResult>, cancel: bool)
        -> (ManagedOAuthResourceTemplate, Arc<Backend>)
    {
        let backend = Arc::new(Backend { calls: Mutex::new(Vec::new()), responses: Mutex::new(responses.into()), cancel });
        let forwarder = Arc::new(Forwarder {
            backend: backend.clone(), next_id: Arc::new(AtomicU64::new(1)),
            limits: ManagedCoreLimits::default(),
        });
        let template = build_templates(forwarder, vec![definition("report://monthly/{name}")])
            .unwrap().pop().unwrap();
        (template, backend)
    }

    #[test]
    fn template_catalog_retains_identity_metadata_and_explicit_cache_hints() {
        let (template, _) = fixture(vec![], false);
        assert_eq!(serde_json::to_value(template.final_template_definition().unwrap()).unwrap(),
            serde_json::to_value(definition("report://monthly/{name}")).unwrap());
        assert_eq!(template.template().unwrap().uri_template, "report://monthly/{name}");
        assert!(template.final_definition().is_none());
        assert!(!template.declares_final_mrtr());
        assert_eq!(template.final_resource_read_cache_hint_provenance(), FinalResourceReadCacheHintProvenance::Explicit);
    }

    #[test]
    fn matched_read_forwards_exact_uri_and_retains_complete_result() {
        let expected = CoreResult::Final(result()).encode().unwrap();
        let (template, backend) = fixture(vec![result()], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let handler: &dyn ResourceHandler = &template;
        let result = block_on(handler.read_final_async_with_uri_in_request(
            &ctx, &cx, "report://monthly/alpha", &UriParams::new(),
        )).unwrap();
        assert_eq!(CoreResult::Final(FinalCoreResult::ResourcesRead { result, diagnostic: None }).encode().unwrap(), expected);
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["uri"], "report://monthly/alpha");
        assert_eq!(calls[0].1["_meta"][fastmcp_protocol::FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
    }

    #[test]
    fn captured_parameters_cannot_replace_or_double_decode_the_wire_uri() {
        let (template, backend) = fixture(vec![result()], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        let params = UriParams::from([("name".to_owned(), "../../private".to_owned())]);
        let uri = "report://monthly/a%252Fb";
        assert!(block_on(template.read_final_async_with_uri(&ctx, uri, &params)).is_ok());
        assert_eq!(backend.calls.lock().unwrap()[0].1["uri"], uri);
    }

    #[test]
    fn out_of_route_uri_rejects_without_ids_or_backend_effects() {
        for uri in ["report://private/alpha", "report://monthly/alpha/extra", "report://monthly/%", "https://example.com/alpha"] {
            let (template, backend) = fixture(vec![result()], false);
            let ctx = McpContext::new(Cx::for_testing(), 1);
            assert!(block_on(template.read_final_async_with_uri(&ctx, uri, &UriParams::new())).is_err(), "{uri}");
            assert!(backend.calls.lock().unwrap().is_empty());
            assert_eq!(backend.responses.lock().unwrap().len(), 1);
            assert_eq!(template.forwarder.next_id.load(Ordering::Relaxed), 1);
        }
    }

    #[test]
    fn variable_scheme_cannot_turn_a_mediated_route_into_client_direct_https() {
        let (template, backend) = fixture(vec![result(), result()], false);
        let template = build_templates(template.forwarder, vec![definition("{scheme}://monthly/{name}")])
            .unwrap().pop().unwrap();
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(template.read_final_async_with_uri(&ctx, "report://monthly/alpha", &UriParams::new())).is_ok());
        let next_id = template.forwarder.next_id.load(Ordering::Relaxed);
        for uri in ["https://monthly/alpha", "HTTPS://monthly/alpha"] {
            // The matcher alone admits this. Only the runtime URI-use policy
            // can refuse it; the negative differs from the positive by scheme.
            assert!(template.matcher.match_uri(uri).unwrap().is_some());
            assert!(block_on(template.read_final_async_with_uri(&ctx, uri, &UriParams::new())).is_err());
        }
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        assert_eq!(backend.responses.lock().unwrap().len(), 1);
        assert_eq!(template.forwarder.next_id.load(Ordering::Relaxed), next_id);
    }

    #[test]
    fn duplicate_unroutable_and_https_templates_reject_the_whole_collection() {
        let (template, backend) = fixture(vec![], false);
        let first = definition("report://monthly/{name}");
        for bad in [first.clone(), definition("report://monthly/{name:3}"), definition("https://example.com/{name}")] {
            assert!(build_templates(Arc::clone(&template.forwarder), vec![first.clone(), bad]).is_err());
        }
        // Duplicate display names do not collide when route identities differ.
        assert_eq!(build_templates(template.forwarder, vec![first, definition("report://yearly/{name}")]).unwrap().len(), 2);
        assert!(backend.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn all_uri_aware_async_hooks_use_the_forwarder() {
        let (template, backend) = fixture(vec![result(); 4], false);
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let params = UriParams::new();
        let uri = "report://monthly/alpha";
        assert!(block_on(template.read_final_async_with_uri(&ctx, uri, &params)).is_ok());
        assert!(matches!(block_on(template.read_final_outcome_async_with_uri(&ctx, uri, &params)).unwrap(), FinalMethodOutcome::Complete(_)));
        assert!(block_on(template.read_final_async_with_uri_in_request(&ctx, &cx, uri, &params)).is_ok());
        assert!(matches!(block_on(template.read_final_outcome_async_with_uri_in_request(&ctx, &cx, uri, &params)).unwrap(), FinalMethodOutcome::Complete(_)));
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 4);
        for (index, (id, _)) in calls.iter().enumerate() {
            assert!(calls[..index].iter().all(|(previous, _)| !id.correlates_with(previous)));
        }
    }

    #[test]
    fn cancellation_prevents_dispatch_or_withholds_late_results() {
        for late in [false, true] {
            let (template, backend) = fixture(vec![result()], late);
            let ctx = McpContext::new(Cx::for_testing(), 1);
            if !late { ctx.request_cancellation().cancel(); }
            assert!(block_on(template.read_final_async_with_uri(&ctx, "report://monthly/alpha", &UriParams::new())).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), usize::from(late));
        }
    }

    #[test]
    fn input_required_is_not_replayed_or_reported_as_complete() {
        let request = core_request("resources/read", json!({"uri":"report://monthly/alpha"}), None).unwrap();
        let CoreResult::Final(input) = request.decode_result(r#"{"resultType":"input_required","requestState":"opaque"}"#).unwrap()
            else { panic!("expected final input") };
        let (template, backend) = fixture(vec![input, result()], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(template.read_final_outcome_async_with_uri(&ctx, "report://monthly/alpha", &UriParams::new())).is_err());
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        assert_eq!(backend.responses.lock().unwrap().len(), 1);
    }

    #[test]
    fn legacy_unexpanded_and_subscription_calls_do_not_start_network_work() {
        let (template, backend) = fixture(vec![result()], false);
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(template.read(&ctx).is_err());
        assert!(block_on(template.read_async(&ctx)).is_err());
        assert!(block_on(template.read_final_async(&ctx)).is_err());
        assert!(template.on_subscribe(&ctx, "report://monthly/alpha").is_err());
        assert!(template.on_unsubscribe(&ctx, "report://monthly/alpha").is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
