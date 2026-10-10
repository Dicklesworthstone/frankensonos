//! Registered schema resources on the normal server tool execution path.
//!
//! This adapter freezes self-contained final input/output schemas from an
//! explicit local registry. Router registration still owns schema admission,
//! framework-error templates, input validation, output validation and catalog
//! mutation. No upstream-schema bypass, new validator or runtime is introduced.
//! Every execution hook is forwarded to the wrapped handler, including its
//! async request-owned and MRTR-resume hooks. Exact-2024 definitions and calls
//! retain their original schema and behavior; only the final definition changes.

use std::fmt;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpOutcome, McpResult};
use fastmcp_protocol::common_types::{OpenMetadata, RawIcon};
use fastmcp_protocol::{
    CompleteResult, Content, FinalCallToolResult, FinalTool, Icon,
    SchemaRegistryError, SchemaResourceRegistry, Tool, ToolAnnotations, admit_final_schema,
};
use serde_json::Value;

use crate::bidirectional::MrtrCompletedInputs;
use crate::handler::{BoxFuture, FinalToolOutcome, ToolErrorKind, ToolExecutionMode, ToolHandler};

/// Construction does not register a tool or invoke an application call hook.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RegisteredSchemaToolError {
    MissingFinalDefinition,
    NameMismatch,
    UpstreamSchemaAuthority,
    InputMustBeObject,
    Schema(SchemaRegistryError),
}
impl fmt::Display for RegisteredSchemaToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingFinalDefinition => f.write_str("handler has no exact final tool definition"),
            Self::NameMismatch => f.write_str("final tool name differs from its handler identity"),
            Self::UpstreamSchemaAuthority => f.write_str("registered schemas require locally owned tool validation"),
            Self::InputMustBeObject => f.write_str("registered tool input schema must declare type object"),
            Self::Schema(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for RegisteredSchemaToolError {}
impl From<SchemaRegistryError> for RegisteredSchemaToolError {
    fn from(error: SchemaRegistryError) -> Self { Self::Schema(error) }
}

/// A normal `ToolHandler` with immutable, registry-backed final schemas.
///
/// Register the returned adapter through the same ServerBuilder/Router tool
/// API as any other handler. Successful construction alone does not authorize
/// invocation or bypass registration. In particular, an output-schema handler
/// must still supply both valid framework-error templates through
/// `final_tool_error_structured_content`; the router rejects it otherwise.
/// Calling trait hooks directly has the same obligations as any ToolHandler:
/// schema and authorization enforcement belong to router dispatch.
///
/// There is no mutable access to the wrapped handler or compiled definition.
/// Interior state, runtime work, cancellation and timeout policy remain owned
/// by the original handler and the caller's request context.
pub struct RegisteredSchemaTool<H> {
    handler: H,
    legacy: Tool,
    definition: FinalTool,
}

impl<H: ToolHandler> RegisteredSchemaTool<H> {
    /// Uses an explicit final definition, including for a legacy-first macro
    /// handler that does not implement `final_definition` itself. Names must
    /// match. All non-schema final fields are retained without projection.
    ///
    /// `input_identity` selects the registry's input root. A supplied output
    /// identity replaces only `outputSchema`; `None` PRESERVES and admits any
    /// output schema already in the final definition rather than erasing it.
    /// Registry references never grant permission to fetch network/file data.
    pub fn new(
        handler: H,
        mut definition: FinalTool,
        registry: &SchemaResourceRegistry,
        input_identity: &str,
        output_identity: Option<&str>,
    ) -> Result<Self, RegisteredSchemaToolError> {
        if handler.upstream_final_tool_schema_registration().is_some() {
            return Err(RegisteredSchemaToolError::UpstreamSchemaAuthority);
        }
        let legacy = handler.definition();
        if legacy.name != definition.name { return Err(RegisteredSchemaToolError::NameMismatch); }
        let input = registry.compile(input_identity)?;
        if input.schema().get("type").and_then(Value::as_str) != Some("object") {
            return Err(RegisteredSchemaToolError::InputMustBeObject);
        }
        let output = match output_identity {
            Some(identity) => Some(registry.compile(identity)?.schema().clone()),
            None => match definition.output_schema.take() {
                Some(schema) if !schema.is_object() => return Err(SchemaRegistryError::InvalidSchema.into()),
                Some(schema) => Some(admit_final_schema(schema)
                    .map_err(SchemaRegistryError::from)?.schema().clone()),
                None => None,
            },
        };
        definition.input_schema = input.schema().clone();
        definition.output_schema = output;
        Ok(Self { handler, legacy, definition })
    }

    /// Uses the handler's exact final catalog metadata. Legacy-first handlers
    /// may instead supply that metadata explicitly to `new`.
    pub fn from_handler(
        handler: H,
        registry: &SchemaResourceRegistry,
        input_identity: &str,
        output_identity: Option<&str>,
    ) -> Result<Self, RegisteredSchemaToolError> {
        let definition = handler.final_definition()
            .ok_or(RegisteredSchemaToolError::MissingFinalDefinition)?;
        Self::new(handler, definition, registry, input_identity, output_identity)
    }

    pub fn compiled_definition(&self) -> &FinalTool { &self.definition }
}

impl<H: ToolHandler> ToolHandler for RegisteredSchemaTool<H> {
    fn definition(&self) -> Tool { self.legacy.clone() }
    fn icon(&self) -> Option<&Icon> { self.handler.icon() }
    fn version(&self) -> Option<&str> { self.handler.version() }
    fn tags(&self) -> &[String] { self.handler.tags() }
    fn annotations(&self) -> Option<&ToolAnnotations> { self.handler.annotations() }
    fn output_schema(&self) -> Option<Value> { self.handler.output_schema() }
    fn final_title(&self) -> Option<&str> { self.definition.title.as_deref() }
    fn final_icons(&self) -> Option<&[RawIcon]> { self.definition.icons.as_deref() }
    fn final_metadata(&self) -> Option<&OpenMetadata> { self.definition.meta.as_ref() }
    fn final_definition(&self) -> Option<FinalTool> { Some(self.definition.clone()) }
    // Deliberately keep ToolHandler's LOCAL authority defaults. A sealed proxy
    // registration cannot be carried across a locally replaced schema.
    fn final_tool_error_structured_content(&self, kind: ToolErrorKind) -> Option<Value> {
        self.handler.final_tool_error_structured_content(kind)
    }
    fn timeout(&self) -> Option<Duration> { self.handler.timeout() }
    fn execution_mode(&self) -> ToolExecutionMode { self.handler.execution_mode() }
    fn declares_final_tasks(&self) -> bool { self.handler.declares_final_tasks() }
    fn declares_final_mrtr(&self) -> bool { self.handler.declares_final_mrtr() }

    fn call(&self, ctx: &McpContext, arguments: Value) -> McpResult<Vec<Content>> {
        self.handler.call(ctx, arguments)
    }
    fn call_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value)
        -> BoxFuture<'a, McpOutcome<Vec<Content>>>
    {
        self.handler.call_async(ctx, arguments)
    }
    fn call_final(&self, ctx: &McpContext, arguments: Value)
        -> McpResult<CompleteResult<FinalCallToolResult>>
    {
        self.handler.call_final(ctx, arguments)
    }
    fn call_final_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value)
        -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>>
    {
        self.handler.call_final_async(ctx, arguments)
    }
    fn call_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
    ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
        self.handler.call_async_in_request(ctx, request_cx, arguments)
    }
    fn call_final_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>> {
        self.handler.call_final_async_in_request(ctx, request_cx, arguments)
    }
    fn call_final_outcome(&self, ctx: &McpContext, arguments: Value) -> McpResult<FinalToolOutcome> {
        self.handler.call_final_outcome(ctx, arguments)
    }
    fn call_final_outcome_async<'a>(&'a self, ctx: &'a McpContext, arguments: Value)
        -> BoxFuture<'a, McpOutcome<FinalToolOutcome>>
    {
        self.handler.call_final_outcome_async(ctx, arguments)
    }
    fn call_final_outcome_async_in_request<'a>(
        &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        self.handler.call_final_outcome_async_in_request(ctx, request_cx, arguments)
    }
    fn call_final_outcome_async_resuming_in_request<'a>(
        &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
        resume_inputs: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        self.handler.call_final_outcome_async_resuming_in_request(ctx, request_cx, arguments, resume_inputs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use fastmcp_core::{Outcome, SessionState};
    use fastmcp_protocol::{JsonRpcRequest, ResultMeta};
    use serde_json::json;
    use crate::Router;

    const INPUT: &str = "https://schemas.example/input";
    const OUTPUT: &str = "https://schemas.example/output";
    const INTEGER: &str = "https://schemas.example/integer";

    fn registry() -> SchemaResourceRegistry {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(INPUT, json!({"type":"object", "properties":{"value":{"$ref":INTEGER}},
            "required":["value"], "additionalProperties":false})).unwrap();
        registry.insert(OUTPUT, json!({"type":"object", "oneOf":[
            {"properties":{"value":{"$ref":INTEGER}}, "required":["value"], "additionalProperties":false},
            {"properties":{"error":{"enum":["invalid_input","handler_error"]}},
                "required":["error"], "additionalProperties":false}
        ]})).unwrap();
        registry.insert(INTEGER, json!({"type":"integer", "minimum":1})).unwrap();
        registry
    }
    fn definition() -> FinalTool {
        serde_json::from_value(json!({"name":"registered", "title":"Display title",
            "description":"Preserve final metadata", "inputSchema":{"type":"object"},
            "annotations":{"title":"Annotation title", "readOnlyHint":true},
            "_meta":{"com.example/source":{"revision":7}}})).unwrap()
    }
    struct Handler {
        effects: Arc<AtomicUsize>,
        owned_calls: Arc<AtomicUsize>,
        payload: Value,
        is_error: bool,
        mapper: bool,
    }
    fn handler(payload: Value) -> Handler {
        Handler { effects: Arc::new(AtomicUsize::new(0)), owned_calls: Arc::new(AtomicUsize::new(0)),
            payload, is_error: false, mapper: true }
    }
    impl ToolHandler for Handler {
        fn definition(&self) -> Tool {
            Tool { name:"registered".to_owned(), description:Some("Legacy definition".to_owned()),
                input_schema:json!({"type":"object"}), output_schema:None, icon:None,
                version:Some("legacy-version".to_owned()), tags:vec!["local".to_owned()], annotations:None }
        }
        fn final_definition(&self) -> Option<FinalTool> { Some(definition()) }
        fn execution_mode(&self) -> ToolExecutionMode { ToolExecutionMode::Async }
        fn timeout(&self) -> Option<Duration> { Some(Duration::from_secs(2)) }
        fn final_tool_error_structured_content(&self, kind: ToolErrorKind) -> Option<Value> {
            self.mapper.then(|| json!({"error":match kind {
                ToolErrorKind::InputValidation => "invalid_input",
                ToolErrorKind::Handler => "handler_error",
            }}))
        }
        fn call(&self, _ctx: &McpContext, _arguments: Value) -> McpResult<Vec<Content>> {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(vec![Content::Text { text:"legacy-result".to_owned() }])
        }
        fn call_final(&self, _ctx: &McpContext, _arguments: Value)
            -> McpResult<CompleteResult<FinalCallToolResult>>
        {
            self.effects.fetch_add(1, Ordering::SeqCst);
            Ok(CompleteResult::new(FinalCallToolResult { content:vec![], is_error:self.is_error,
                structured_content:Some(self.payload.clone()) }, ResultMeta::empty()))
        }
        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self, ctx: &'a McpContext, request_cx: &'a Cx, arguments: Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            self.owned_calls.fetch_add(1, Ordering::SeqCst);
            assert!(resume_inputs.is_none(), "ordinary test call has no admitted continuation");
            request_cx.checkpoint().expect("the forwarded request-owned context remains live");
            Box::pin(async move {
                match self.call_final(ctx, arguments) {
                    Ok(result) => Outcome::Ok(FinalToolOutcome::Complete(result)),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }
    fn adapter(handler: Handler) -> RegisteredSchemaTool<Handler> {
        RegisteredSchemaTool::from_handler(handler, &registry(), INPUT, Some(OUTPUT)).unwrap()
    }
    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .blocking_threads(0, 2).build().unwrap()
    }
    async fn call(router: Arc<Router>, id: i64, arguments: Value) -> McpResult<Value> {
        let cx = Cx::current().expect("test runtime owns request authority");
        let ctx = McpContext::with_state(cx, id as u64, SessionState::new());
        router.dispatch_stateless_owned(ctx, JsonRpcRequest::new("tools/call", Some(json!({
            "name":"registered", "arguments":arguments,
            "_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28",
                "io.modelcontextprotocol/clientCapabilities":{}},
        })), id)).await
    }

    #[test]
    fn final_metadata_and_exact_legacy_schema_are_preserved() {
        let handler = handler(json!({"value":2}));
        let legacy = serde_json::to_value(handler.definition()).unwrap();
        let tool = adapter(handler);
        assert_eq!(serde_json::to_value(tool.definition()).unwrap(), legacy);
        let mut actual = serde_json::to_value(tool.compiled_definition()).unwrap();
        let mut expected = serde_json::to_value(definition()).unwrap();
        for value in [&mut actual, &mut expected] {
            value.as_object_mut().unwrap().remove("inputSchema");
            value.as_object_mut().unwrap().remove("outputSchema");
        }
        assert_eq!(actual, expected);
        assert_eq!(tool.timeout(), Some(Duration::from_secs(2)));
        assert_eq!(tool.execution_mode(), ToolExecutionMode::Async);
        assert!(tool.upstream_final_tool_schema_registration().is_none());
        let wire = serde_json::to_value(tool.final_definition().unwrap()).unwrap();
        let decoded: FinalTool = serde_json::from_value(wire).unwrap();
        let input = admit_final_schema(decoded.input_schema).unwrap();
        assert!(input.validate(&json!({"value":2})).is_ok());
        assert!(input.validate(&json!({"value":0})).is_err());
    }

    #[test]
    fn absent_output_selection_keeps_the_existing_output_contract() {
        let mut original = definition();
        original.output_schema = Some(json!({"type":"integer", "minimum":5}));
        let tool = RegisteredSchemaTool::new(handler(json!(5)), original.clone(), &registry(), INPUT, None).unwrap();
        assert_eq!(tool.compiled_definition().output_schema, original.output_schema);
        let schema = admit_final_schema(tool.compiled_definition().output_schema.clone().unwrap()).unwrap();
        assert!(schema.validate(&json!(5)).is_ok());
        assert!(schema.validate(&json!(4)).is_err());
    }

    #[test]
    fn constructor_rejects_wrong_identity_unresolved_output_and_nonobject_input() {
        let mut wrong = definition();
        wrong.name = "different".to_owned();
        assert!(matches!(RegisteredSchemaTool::new(handler(json!(1)), wrong, &registry(), INPUT, None),
            Err(RegisteredSchemaToolError::NameMismatch)));
        assert!(matches!(RegisteredSchemaTool::from_handler(handler(json!(1)), &registry(), INPUT, Some("https://missing.example/schema")),
            Err(RegisteredSchemaToolError::Schema(SchemaRegistryError::UnknownResource))));
        assert!(matches!(RegisteredSchemaTool::from_handler(handler(json!(1)), &registry(), INTEGER, None),
            Err(RegisteredSchemaToolError::InputMustBeObject)));
        let mut invalid = definition();
        invalid.output_schema = Some(json!({"$ref":"https://missing.example/schema"}));
        assert!(RegisteredSchemaTool::new(handler(json!(1)), invalid, &registry(), INPUT, None).is_err());
        let mut invalid = definition();
        invalid.output_schema = Some(json!(true));
        assert!(RegisteredSchemaTool::new(handler(json!(1)), invalid, &registry(), INPUT, None).is_err());
    }

    #[test]
    fn router_validates_registered_input_before_any_handler_effect() {
        let handler = handler(json!({"value":2}));
        let effects = handler.effects.clone();
        let owned = handler.owned_calls.clone();
        let mut router = Router::new();
        router.add_tool(adapter(handler)).unwrap();
        runtime().block_on(async {
            let router = Arc::new(router);
            let valid = call(router.clone(), 1, json!({"value":2})).await.unwrap();
            assert_eq!(valid["resultType"], "complete");
            assert_eq!(valid["structuredContent"], json!({"value":2}));
            assert_eq!(effects.load(Ordering::SeqCst), 1);
            assert_eq!(owned.load(Ordering::SeqCst), 1);
            let invalid = call(router, 2, json!({"value":0})).await.unwrap();
            assert_eq!(invalid["isError"], true);
            assert_eq!(invalid["structuredContent"], json!({"error":"invalid_input"}));
            assert_eq!(effects.load(Ordering::SeqCst), 1);
            assert_eq!(owned.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn router_refuses_invalid_structured_output_after_one_effect() {
        let handler = handler(json!({"value":0}));
        let effects = handler.effects.clone();
        let mut router = Router::new();
        router.add_tool(adapter(handler)).unwrap();
        runtime().block_on(async {
            let result = call(Arc::new(router), 1, json!({"value":2})).await;
            assert!(result.is_err(), "invalid structured output must not become a successful result");
            assert_eq!(effects.load(Ordering::SeqCst), 1, "validation does not replay or undo the handler");
        });
    }

    #[test]
    fn router_requires_framework_error_output_before_catalog_mutation() {
        let mut handler = handler(json!({"value":2}));
        handler.mapper = false;
        let effects = handler.effects.clone();
        let mut router = Router::new();
        assert!(router.add_tool(adapter(handler)).is_err());
        assert_eq!(router.tools_count(), 0);
        assert_eq!(effects.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn tool_error_results_retain_their_output_validation_obligation() {
        for (value, accepted) in [("handler_error", true), ("not_in_schema", false)] {
            let mut handler = handler(json!({"error":value}));
            handler.is_error = true;
            let effects = handler.effects.clone();
            let mut router = Router::new();
            router.add_tool(adapter(handler)).unwrap();
            runtime().block_on(async {
                let result = call(Arc::new(router), 1, json!({"value":2})).await;
                if accepted { assert_eq!(result.unwrap()["isError"], true); }
                else { assert!(result.is_err()); }
                assert_eq!(effects.load(Ordering::SeqCst), 1);
            });
        }
    }
}
