//! Completion adapters bound to already-admitted managed OAuth handlers.
//!
//! A completion route retains the same upstream login, request limits and ID
//! allocator as its prompt or resource-template handler. It cannot select an
//! arbitrary upstream reference or undisclosed argument. Register each adapter
//! with the builder's target-specific completion registration so ordinary
//! router visibility and authorization still apply.
//!
//! ```ignore
//! // `provider` is the application's ManagedOAuthProvider; `cx` is borrowed
//! // from its runtime. Register only the upstream handlers the gateway permits.
//! for prompt in provider.prompts(cx).await? {
//!     let name = prompt.catalog_definition().name.clone();
//!     let completion = prompt.completion_handler()?;
//!     builder = builder.prompt(prompt).prompt_completion_handler(name, completion);
//! }
//! for template in provider.resource_templates(cx).await? {
//!     let uri = template.catalog_definition().uri_template.clone();
//!     let completion = template.completion_handler()?;
//!     builder = builder.resource(template).resource_template_completion_handler(uri, completion);
//! }
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpError, McpOutcome, McpResult};
use fastmcp_protocol::{
    CompletionValues, FinalCompletionParams, FinalCompletionReference,
    FinalCompletionValues, FinalCoreResult, LegacyCompletionParams, UriTemplatePart,
};
use serde_json::{Value, json};

use super::ManagedOAuthResourceTemplate;
use super::super::{Forwarder, MODERN_ASYNC_ONLY, ManagedOAuthPrompt, UNEXPECTED_RESULT, check_cx, outcome};
use crate::handler::{BoxFuture, CompletionHandler};

impl ManagedOAuthPrompt {
    /// Binds completion to this exact published prompt and its declared args.
    ///
    /// Construct before moving the prompt into `builder.prompt`, then register
    /// with `builder.prompt_completion_handler(published_name, completion)`.
    /// Only the name is rewritten upstream; optional reference titles, argument
    /// values and admitted context are retained. Construction performs no I/O.
    pub fn completion_handler(&self) -> McpResult<ManagedOAuthCompletion> {
        let mut arguments = HashSet::new();
        if let Some(declared) = &self.definition.arguments {
            for argument in declared {
                admit_argument(&mut arguments, &argument.name)?;
            }
        }
        Ok(ManagedOAuthCompletion {
            forwarder: Arc::clone(&self.forwarder),
            target: CompletionTarget::Prompt {
                published_name: self.definition.name.clone(),
                upstream_name: self.upstream_name.clone(),
            },
            arguments,
        })
    }
}

impl ManagedOAuthResourceTemplate {
    /// Binds completion to this exact URI template and its parsed variables.
    ///
    /// Register with `builder.resource_template_completion_handler(uri, handler)`.
    /// The reference must be the template itself, not an expanded resource URI.
    /// No extra catalog fetch, URL fetch, or credential acquisition is performed.
    pub fn completion_handler(&self) -> McpResult<ManagedOAuthCompletion> {
        let mut arguments = HashSet::new();
        for part in self.matcher.template().parts() {
            if let UriTemplatePart::Expression(expression) = part {
                for variable in expression.variables() {
                    admit_argument(&mut arguments, variable.name())?;
                }
            }
        }
        Ok(ManagedOAuthCompletion {
            forwarder: Arc::clone(&self.forwarder),
            target: CompletionTarget::Resource {
                uri_template: self.definition.uri_template.clone(),
            },
            arguments,
        })
    }
}

fn admit_argument(arguments: &mut HashSet<String>, name: &str) -> McpResult<()> {
    if name.is_empty() || !arguments.insert(name.to_owned()) {
        return Err(McpError::invalid_request("Managed OAuth completion arguments are empty or duplicated"));
    }
    Ok(())
}

enum CompletionTarget {
    Prompt { published_name: String, upstream_name: String },
    Resource { uri_template: String },
}

/// One modern completion route minted by a managed prompt or template handler.
///
/// There is no public constructor accepting a bare upstream identity. Multiple
/// providers can register separate adapters without sharing credentials or
/// routing a request through a global fallback. This adapter returns the
/// completion payload required by `CompletionHandler`; the router constructs
/// its result envelope. It does not relay unknown top-level result members.
pub struct ManagedOAuthCompletion {
    forwarder: Arc<Forwarder>,
    target: CompletionTarget,
    arguments: HashSet<String>,
}

impl ManagedOAuthCompletion {
    fn parameters(&self, params: FinalCompletionParams) -> McpResult<Value> {
        let reference = match (&self.target, params.reference) {
            (CompletionTarget::Prompt { published_name, upstream_name },
                FinalCompletionReference::Prompt { name }) if name == *published_name => {
                FinalCompletionReference::Prompt { name: upstream_name.clone() }
            }
            (CompletionTarget::Prompt { published_name, upstream_name },
                FinalCompletionReference::PromptWithTitle { name, title }) if name == *published_name => {
                FinalCompletionReference::PromptWithTitle { name: upstream_name.clone(), title }
            }
            (CompletionTarget::Resource { uri_template },
                FinalCompletionReference::Resource { uri }) if uri == *uri_template => {
                FinalCompletionReference::Resource { uri }
            }
            _ => return Err(unregistered_target()),
        };
        if !self.arguments.contains(&params.argument.name) {
            return Err(unregistered_target());
        }
        if let Some(arguments) = params.context.as_ref().and_then(|context| context.arguments.as_ref())
            && arguments.keys().any(|name| !self.arguments.contains(name))
        {
            return Err(unregistered_target());
        }
        // Do not serialize downstream _meta at all. Forwarder reconstructs the
        // protocol metadata and progress marker from this request's context.
        let reference = serde_json::to_value(reference).map_err(|_| unregistered_target())?;
        let mut parameters = json!({"ref": reference, "argument": params.argument});
        if let Some(context) = params.context {
            // The protocol serializer enforces context count/key/value/byte
            // bounds. Keep absent, present-empty, and populated states distinct.
            parameters["context"] = serde_json::to_value(context)
                .map_err(|_| McpError::invalid_params("Invalid managed OAuth completion context"))?;
        }
        Ok(parameters)
    }

    async fn invoke(
        &self, ctx: &McpContext, cx: &Cx, params: FinalCompletionParams,
    ) -> McpResult<FinalCompletionValues> {
        ctx.checkpoint()?;
        check_cx(cx)?;
        let parameters = self.parameters(params)?;
        match self.forwarder.execute(ctx, cx, "completion/complete", parameters).await? {
            FinalCoreResult::Completion { result, .. } => Ok(result.payload.completion),
            _ => Err(McpError::invalid_request(UNEXPECTED_RESULT)),
        }
    }
}

fn unregistered_target() -> McpError {
    McpError::invalid_params("Completion reference or argument is not registered for this managed OAuth handler")
}

impl CompletionHandler for ManagedOAuthCompletion {
    fn complete_legacy(&self, _ctx: &McpContext, _params: LegacyCompletionParams) -> McpResult<CompletionValues> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn complete_final(&self, _ctx: &McpContext, _params: FinalCompletionParams) -> McpResult<FinalCompletionValues> {
        Err(McpError::invalid_request(MODERN_ASYNC_ONLY))
    }
    fn complete_final_async<'a>(
        &'a self, ctx: &'a McpContext, params: FinalCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<FinalCompletionValues>> {
        Box::pin(async move { outcome(self.invoke(ctx, ctx.cx(), params).await) })
    }
    fn complete_final_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, cx: &'a Cx, params: FinalCompletionParams,
    ) -> BoxFuture<'a, McpOutcome<FinalCompletionValues>> {
        Box::pin(async move { outcome(self.invoke(ctx, cx, params).await) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicU64, Ordering};
    use fastmcp_core::block_on;
    use fastmcp_client::http_auth::rpc::ManagedCoreLimits;
    use fastmcp_protocol::{CoreRequest, CoreResult, FinalPrompt, FinalResourceTemplate, RequestId};
    use super::super::build_templates;
    use super::super::super::{CoreBackend, build_prompts, core_request};

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
    fn params(reference: Value, argument: &str) -> FinalCompletionParams {
        serde_json::from_value(json!({
            "_meta": {"authorization":"Bearer downstream-secret"},
            "ref":reference,"argument":{"name":argument,"value":"al"}
        })).unwrap()
    }
    fn prompt_params() -> FinalCompletionParams {
        params(json!({"type":"ref/prompt","name":"remote/summarize"}), "report")
    }
    fn result() -> FinalCoreResult {
        let request = core_request("completion/complete", json!({
            "ref":{"type":"ref/prompt","name":"summarize"},
            "argument":{"name":"report","value":"al"}
        }), None).unwrap();
        let CoreResult::Final(result) = request.decode_result(r#"{
            "resultType":"complete","completion":{"values":["alpha","alpine"],"total":42,"hasMore":true}
        }"#).unwrap() else { panic!("expected final completion") };
        result
    }
    fn fixture(responses: Vec<FinalCoreResult>, cancel: bool)
        -> (ManagedOAuthPrompt, ManagedOAuthResourceTemplate, Arc<Backend>)
    {
        let backend = Arc::new(Backend { calls: Mutex::new(Vec::new()), responses: Mutex::new(responses.into()), cancel });
        let forwarder = Arc::new(Forwarder {
            backend: backend.clone(), next_id: Arc::new(AtomicU64::new(1)), limits: ManagedCoreLimits::default(),
        });
        let prompt: FinalPrompt = serde_json::from_value(json!({
            "name":"summarize","arguments":[{"name":"report"},{"name":"style"}]
        })).unwrap();
        let template: FinalResourceTemplate = serde_json::from_value(json!({
            "name":"Report","uriTemplate":"report://monthly/{year}/{name}"
        })).unwrap();
        let prompt = build_prompts(Arc::clone(&forwarder), vec![prompt], Some("remote")).unwrap().pop().unwrap();
        let template = build_templates(forwarder, vec![template]).unwrap().pop().unwrap();
        (prompt, template, backend)
    }

    #[test]
    fn prompt_completion_keeps_title_context_and_values_but_rebuilds_metadata() {
        let (prompt, _, backend) = fixture(vec![result()], false);
        let completion = prompt.completion_handler().unwrap();
        let mut parameters = params(json!({"type":"ref/prompt","name":"remote/summarize","title":"Display title"}), "report");
        parameters.context = Some(serde_json::from_value(json!({"arguments":{"style":"Unicode: 日本語"}})).unwrap());
        let cx = Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let handler: &dyn CompletionHandler = &completion;
        let values = block_on(handler.complete_final_async_in_request(&ctx, &cx, parameters)).unwrap();
        assert_eq!(serde_json::to_value(values).unwrap(), json!({"values":["alpha","alpine"],"total":42,"hasMore":true}));
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["ref"], json!({"type":"ref/prompt","name":"summarize","title":"Display title"}));
        assert_eq!(calls[0].1["argument"], json!({"name":"report","value":"al"}));
        assert_eq!(calls[0].1["context"], json!({"arguments":{"style":"Unicode: 日本語"}}));
        assert_eq!(calls[0].1["_meta"][fastmcp_protocol::FINAL_CLIENT_CAPABILITIES_META_KEY], json!({}));
        assert!(!calls[0].1.to_string().contains("downstream-secret"));
    }

    #[test]
    fn template_completion_uses_exact_template_and_declared_variables() {
        let (_, template, backend) = fixture(vec![result()], false);
        let completion = template.completion_handler().unwrap();
        let mut parameters = params(json!({"type":"ref/resource","uri":"report://monthly/{year}/{name}"}), "name");
        parameters.context = Some(serde_json::from_value(json!({"arguments":{"year":"2026"}})).unwrap());
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(completion.complete_final_async(&ctx, parameters)).is_ok());
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].1["ref"]["uri"], "report://monthly/{year}/{name}");
        assert_eq!(calls[0].1["context"]["arguments"]["year"], "2026");
    }

    #[test]
    fn changed_reference_or_argument_never_allocates_an_id_or_calls_upstream() {
        let (prompt, template, backend) = fixture(vec![result()], false);
        let completion = prompt.completion_handler().unwrap();
        let ctx = McpContext::new(Cx::for_testing(), 1);
        for parameters in [
            params(json!({"type":"ref/prompt","name":"summarize"}), "report"),
            params(json!({"type":"ref/prompt","name":"remote/private"}), "report"),
            params(json!({"type":"ref/resource","uri":"report://monthly/{year}/{name}"}), "report"),
            params(json!({"type":"ref/prompt","name":"remote/summarize"}), "private"),
        ] {
            assert!(block_on(completion.complete_final_async(&ctx, parameters)).is_err());
        }
        let completion = template.completion_handler().unwrap();
        let expanded = params(json!({"type":"ref/resource","uri":"report://monthly/2026/alpha"}), "name");
        assert!(block_on(completion.complete_final_async(&ctx, expanded)).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(backend.responses.lock().unwrap().len(), 1);
        assert_eq!(completion.forwarder.next_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn unknown_context_variables_are_refused_before_upstream_work() {
        let (prompt, _, backend) = fixture(vec![result()], false);
        let completion = prompt.completion_handler().unwrap();
        let mut parameters = prompt_params();
        parameters.context = Some(serde_json::from_value(json!({"arguments":{"private":"secret"}})).unwrap());
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(completion.complete_final_async(&ctx, parameters)).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(completion.forwarder.next_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn absent_and_present_empty_completion_contexts_remain_distinct() {
        let (prompt, _, _) = fixture(vec![], false);
        let completion = prompt.completion_handler().unwrap();
        assert!(completion.parameters(prompt_params()).unwrap().get("context").is_none());
        for context in [json!({}), json!({"arguments":{}})] {
            let mut parameters = prompt_params();
            parameters.context = Some(serde_json::from_value(context.clone()).unwrap());
            assert_eq!(completion.parameters(parameters).unwrap()["context"], context);
        }
    }

    #[test]
    fn completion_context_serializer_bounds_are_enforced_before_io() {
        let (prompt, _, backend) = fixture(vec![result()], false);
        let completion = prompt.completion_handler().unwrap();
        let mut parameters = prompt_params();
        // Build through the public typed API to exercise its outbound bound,
        // rather than letting the inbound decoder reject this fixture first.
        parameters.context = Some(fastmcp_protocol::FinalCompletionContext {
            arguments: Some(std::collections::BTreeMap::from([(
                "style".to_owned(), "x".repeat(fastmcp_protocol::MAX_COMPLETION_CONTEXT_ARGUMENT_VALUE_BYTES + 1),
            )])),
        });
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(completion.complete_final_async(&ctx, parameters)).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
        assert_eq!(completion.forwarder.next_id.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn adapters_share_their_original_forwarder_and_correlation_allocator() {
        let (prompt, template, backend) = fixture(vec![result(), result()], false);
        let prompt_completion = prompt.completion_handler().unwrap();
        let template_completion = template.completion_handler().unwrap();
        assert!(Arc::ptr_eq(&prompt.forwarder, &prompt_completion.forwarder));
        assert!(Arc::ptr_eq(&template.forwarder, &template_completion.forwarder));
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(prompt_completion.complete_final_async(&ctx, prompt_params())).is_ok());
        let parameters = params(json!({"type":"ref/resource","uri":"report://monthly/{year}/{name}"}), "name");
        assert!(block_on(template_completion.complete_final_async(&ctx, parameters)).is_ok());
        let calls = backend.calls.lock().unwrap();
        assert_eq!(calls.len(), 2);
        assert!(!calls[0].0.correlates_with(&calls[1].0));
    }

    #[test]
    fn separate_upstreams_with_the_same_local_name_retain_distinct_backends() {
        let (first, _, first_backend) = fixture(vec![result()], false);
        let (second, _, second_backend) = fixture(vec![result()], false);
        let first = first.completion_handler().unwrap();
        let second = second.completion_handler().unwrap();
        assert!(!Arc::ptr_eq(&first.forwarder, &second.forwarder));
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(first.complete_final_async(&ctx, prompt_params())).is_ok());
        assert_eq!(first_backend.calls.lock().unwrap().len(), 1);
        assert!(second_backend.calls.lock().unwrap().is_empty());
        assert!(block_on(second.complete_final_async(&ctx, prompt_params())).is_ok());
        assert_eq!(first_backend.calls.lock().unwrap().len(), 1);
        assert_eq!(second_backend.calls.lock().unwrap().len(), 1);
    }

    #[test]
    fn cancellation_before_dispatch_and_during_completion_prevents_delivery() {
        for late in [false, true] {
            let (prompt, _, backend) = fixture(vec![result()], late);
            let completion = prompt.completion_handler().unwrap();
            let ctx = McpContext::new(Cx::for_testing(), 1);
            if !late { ctx.request_cancellation().cancel(); }
            assert!(block_on(completion.complete_final_async(&ctx, prompt_params())).is_err());
            assert_eq!(backend.calls.lock().unwrap().len(), usize::from(late));
        }
    }

    #[test]
    fn wrong_method_result_is_refused_without_retry() {
        let request = core_request("prompts/get", json!({"name":"summarize"}), None).unwrap();
        let CoreResult::Final(wrong) = request.decode_result(r#"{"resultType":"complete","messages":[]}"#).unwrap()
            else { panic!("expected final prompt") };
        let (prompt, _, backend) = fixture(vec![wrong, result()], false);
        let completion = prompt.completion_handler().unwrap();
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(block_on(completion.complete_final_async(&ctx, prompt_params())).is_err());
        assert_eq!(backend.calls.lock().unwrap().len(), 1);
        assert_eq!(backend.responses.lock().unwrap().len(), 1);
    }

    #[test]
    fn ambiguous_prompt_arguments_cannot_mint_completion_routes() {
        for arguments in [json!([{"name":"report"},{"name":"report"}]), json!([{"name":""}])] {
            let (mut prompt, _, backend) = fixture(vec![], false);
            prompt.definition.arguments = Some(serde_json::from_value(arguments).unwrap());
            assert!(prompt.completion_handler().is_err());
            assert!(backend.calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn sync_and_legacy_entry_points_never_start_modern_io() {
        let (prompt, _, backend) = fixture(vec![result()], false);
        let completion = prompt.completion_handler().unwrap();
        let ctx = McpContext::new(Cx::for_testing(), 1);
        assert!(completion.complete_final(&ctx, prompt_params()).is_err());
        let legacy: LegacyCompletionParams = serde_json::from_value(json!({
            "ref":{"type":"ref/prompt","name":"remote/summarize"},"argument":{"name":"report","value":"al"}
        })).unwrap();
        assert!(completion.complete_legacy(&ctx, legacy.clone()).is_err());
        assert!(block_on(completion.complete_legacy_async(&ctx, legacy)).is_err());
        assert!(backend.calls.lock().unwrap().is_empty());
    }
}
