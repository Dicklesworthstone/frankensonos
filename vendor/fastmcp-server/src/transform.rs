//! Tool transformations for dynamic schema modification.
//!
//! This module provides the ability to transform tools dynamically, allowing:
//! - Renaming tools and their arguments
//! - Modifying descriptions
//! - Providing default values for arguments
//! - Hiding arguments from the schema (while still providing values)
//! - Wrapping tools with custom transformation functions
//!
//! # Example
//!
//! ```ignore
//! use fastmcp_server::transform::{ArgTransform, TransformedTool};
//!
//! // Original tool with cryptic argument names
//! let original_tool = my_search_tool();
//!
//! // Transform to be more LLM-friendly
//! let transformed = TransformedTool::from_tool(original_tool)
//!     .name("semantic_search")
//!     .description("Search for documents using natural language")
//!     .transform_arg("q", ArgTransform::new().name("query").description("Search query"))
//!     .transform_arg("n", ArgTransform::new().name("limit").default(10))
//!     .build();
//! ```

use std::collections::HashMap;
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpOutcome, McpResult, Outcome};
use fastmcp_protocol::common_types::{OpenMetadata, RawIcon};
use fastmcp_protocol::{
    CompleteResult, Content, FinalCallToolResult, FinalTool, Icon, Tool, ToolAnnotations,
};

use crate::bidirectional::MrtrCompletedInputs;
use crate::handler::{
    BoxFuture, BoxedToolHandler, FinalToolOutcome, FinalToolSchemaAuthority, ToolErrorKind,
    ToolHandler, UpstreamFinalToolSchemaRegistration,
};

/// Sentinel value for unset optional fields.
#[derive(Debug, Clone, Copy, Default)]
pub struct NotSet;

/// Transformation rules for a single argument.
///
/// Use the builder methods to specify which aspects of the argument to transform.
/// Unspecified fields inherit from the original argument, except that a runtime
/// default makes omission legal unless `required` is explicitly `Some(true)`.
#[derive(Debug, Clone, Default)]
pub struct ArgTransform {
    /// New name for the argument.
    pub name: Option<String>,
    /// New description for the argument.
    pub description: Option<String>,
    /// Default value (as JSON) for the argument.
    pub default: Option<serde_json::Value>,
    /// Whether to hide this argument from the schema.
    /// Hidden arguments must have a default value.
    pub hide: bool,
    /// Override the required status. `Some(true)` requires an explicit value
    /// even with a default; `Some(false)` permits omission.
    pub required: Option<bool>,
    /// New type annotation for the argument (as JSON Schema).
    pub type_schema: Option<serde_json::Value>,
}

impl ArgTransform {
    /// Creates a new empty argument transform.
    #[must_use]
    pub fn new() -> Self {
        <Self as Default>::default()
    }

    /// Sets the new name for this argument.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the new description for this argument.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Sets the default value for this argument.
    ///
    /// The adapter supplies it only when the published argument is absent.
    /// Omission is allowed by the generated schema unless [`Self::required`]
    /// was explicitly selected. Present values, including null, are not replaced.
    #[must_use]
    pub fn default(mut self, value: impl Into<serde_json::Value>) -> Self {
        self.default = Some(value.into());
        self
    }

    /// Sets a string default value.
    #[must_use]
    pub fn default_str(self, value: impl Into<String>) -> Self {
        self.default(serde_json::Value::String(value.into()))
    }

    /// Sets an integer default value.
    #[must_use]
    pub fn default_int(self, value: i64) -> Self {
        self.default(serde_json::Value::Number(value.into()))
    }

    /// Sets a boolean default value.
    #[must_use]
    pub fn default_bool(self, value: bool) -> Self {
        self.default(serde_json::Value::Bool(value))
    }

    /// Hides this argument from the schema.
    ///
    /// Hidden arguments are not exposed to the LLM but must have a default
    /// value that will be used when the tool is called.
    #[must_use]
    pub fn hide(mut self) -> Self {
        self.hide = true;
        self
    }

    /// Makes this argument required (even if it was optional).
    #[must_use]
    pub fn required(mut self) -> Self {
        self.required = Some(true);
        self
    }

    /// Makes this argument optional (even if it was required).
    #[must_use]
    pub fn optional(mut self) -> Self {
        self.required = Some(false);
        self
    }

    /// Sets the JSON Schema type for this argument.
    #[must_use]
    pub fn type_schema(mut self, schema: serde_json::Value) -> Self {
        self.type_schema = Some(schema);
        self
    }

    /// Creates a transform that drops (hides) this argument with a default value.
    #[must_use]
    pub fn drop_with_default(value: impl Into<serde_json::Value>) -> Self {
        Self::new().default(value).hide()
    }
}

/// A transformed tool that wraps another tool and applies transformations.
///
/// Transformations can include:
/// - Renaming the tool
/// - Modifying the description
/// - Transforming arguments (rename, add defaults, hide, etc.)
/// - Applying a custom transformation function
pub struct TransformedTool {
    /// The underlying tool being transformed.
    parent: BoxedToolHandler,
    /// Transformed tool definition.
    definition: Tool,
    /// Argument transformations (keyed by original argument name).
    arg_transforms: HashMap<String, ArgTransform>,
    /// Mapping from new arg names to original arg names.
    name_mapping: HashMap<String, String>,
}

impl TransformedTool {
    /// Creates a builder for transforming an existing tool.
    pub fn from_tool<H: ToolHandler + 'static>(tool: H) -> TransformedToolBuilder {
        TransformedToolBuilder::new(Box::new(tool))
    }

    /// Creates a builder from a boxed tool handler.
    pub fn from_boxed(tool: BoxedToolHandler) -> TransformedToolBuilder {
        TransformedToolBuilder::new(tool)
    }

    /// Returns the parent tool's definition.
    #[must_use]
    pub fn parent_definition(&self) -> Tool {
        self.parent.definition()
    }

    /// Returns the argument transforms.
    #[must_use]
    pub fn arg_transforms(&self) -> &HashMap<String, ArgTransform> {
        &self.arg_transforms
    }

    /// Transforms the incoming arguments (with new names) to the original format.
    fn transform_arguments(&self, arguments: serde_json::Value) -> McpResult<serde_json::Value> {
        let mut args = match arguments {
            serde_json::Value::Object(map) => map,
            serde_json::Value::Null => serde_json::Map::new(),
            _ => {
                return Err(fastmcp_core::McpError::invalid_params(
                    "Arguments must be an object",
                ));
            }
        };

        let mut result = serde_json::Map::new();

        // Consume published names before discarding original-name aliases.
        // An original name can also be another argument's published name in
        // swaps or chains (a -> b, b -> c). Values move without cloning.
        for (original_name, transform) in &self.arg_transforms {
            let new_name = transform.name.as_ref().unwrap_or(original_name);

            if transform.hide {
                // Hidden arguments are server-owned. A caller-supplied value
                // under the published name or the original name must not
                // override the configured default, and cannot substitute for
                // a missing one.
                if let Some(default) = &transform.default {
                    result.insert(original_name.clone(), default.clone());
                    continue;
                }
                return Err(fastmcp_core::McpError::invalid_params(format!(
                    "Hidden argument '{original_name}' requires a default value"
                )));
            }

            if let Some(value) = args.remove(new_name) {
                result.insert(original_name.clone(), value);
            } else if let Some(default) = &transform.default {
                result.insert(original_name.clone(), default.clone());
            }
        }

        // Only unrelated arguments pass through. Original names and aliases
        // are owned by the transformation above: neither may overwrite a
        // mapped value or a server-owned hidden default.
        for (key, value) in args {
            if !self.arg_transforms.contains_key(&key) && !self.name_mapping.contains_key(&key) {
                result.insert(key, value);
            }
        }

        Ok(serde_json::Value::Object(result))
    }
}

impl std::fmt::Debug for TransformedTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransformedTool")
            .field("definition", &self.definition)
            .field("arg_transforms", &self.arg_transforms)
            .finish_non_exhaustive()
    }
}

impl ToolHandler for TransformedTool {
    fn definition(&self) -> Tool {
        self.definition.clone()
    }

    fn icon(&self) -> Option<&Icon> {
        self.parent.icon()
    }

    fn version(&self) -> Option<&str> {
        self.parent.version()
    }

    fn tags(&self) -> &[String] {
        self.parent.tags()
    }

    fn annotations(&self) -> Option<&ToolAnnotations> {
        self.parent.annotations()
    }

    fn output_schema(&self) -> Option<serde_json::Value> {
        self.parent.output_schema()
    }

    fn final_title(&self) -> Option<&str> {
        self.parent.final_title()
    }

    fn final_icons(&self) -> Option<&[RawIcon]> {
        self.parent.final_icons()
    }

    fn final_metadata(&self) -> Option<&OpenMetadata> {
        self.parent.final_metadata()
    }

    fn final_definition(&self) -> Option<FinalTool> {
        let mut definition = self.parent.final_definition()?;
        definition.name.clone_from(&self.definition.name);
        definition
            .description
            .clone_from(&self.definition.description);
        definition.input_schema =
            transform_input_schema(&self.arg_transforms, &definition.input_schema);
        Some(definition)
    }

    fn final_tool_schema_authority(&self) -> FinalToolSchemaAuthority {
        self.parent.final_tool_schema_authority()
    }

    fn upstream_final_tool_schema_registration(
        &self,
    ) -> Option<UpstreamFinalToolSchemaRegistration> {
        self.parent.upstream_final_tool_schema_registration()
    }

    fn final_tool_error_structured_content(
        &self,
        kind: ToolErrorKind,
    ) -> Option<serde_json::Value> {
        self.parent.final_tool_error_structured_content(kind)
    }

    fn declares_final_tasks(&self) -> bool {
        self.parent.declares_final_tasks()
    }

    fn declares_final_mrtr(&self) -> bool {
        self.parent.declares_final_mrtr()
    }

    fn timeout(&self) -> Option<Duration> {
        self.parent.timeout()
    }

    fn execution_mode(&self) -> crate::ToolExecutionMode {
        self.parent.execution_mode()
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        let transformed_args = self.transform_arguments(arguments)?;
        self.parent.call(ctx, transformed_args)
    }

    fn call_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent.call_async(ctx, transformed_args).await
        })
    }

    fn call_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        request_cx: &'a Cx,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<Vec<Content>>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent
                .call_async_in_request(ctx, request_cx, transformed_args)
                .await
        })
    }

    fn call_final(
        &self,
        ctx: &McpContext,
        arguments: serde_json::Value,
    ) -> McpResult<CompleteResult<FinalCallToolResult>> {
        self.parent
            .call_final(ctx, self.transform_arguments(arguments)?)
    }

    fn call_final_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent.call_final_async(ctx, transformed_args).await
        })
    }

    fn call_final_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        request_cx: &'a Cx,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<CompleteResult<FinalCallToolResult>>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent
                .call_final_async_in_request(ctx, request_cx, transformed_args)
                .await
        })
    }

    fn call_final_outcome(
        &self,
        ctx: &McpContext,
        arguments: serde_json::Value,
    ) -> McpResult<FinalToolOutcome> {
        self.parent
            .call_final_outcome(ctx, self.transform_arguments(arguments)?)
    }

    fn call_final_outcome_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent
                .call_final_outcome_async(ctx, transformed_args)
                .await
        })
    }

    fn call_final_outcome_async_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        request_cx: &'a Cx,
        arguments: serde_json::Value,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent
                .call_final_outcome_async_in_request(ctx, request_cx, transformed_args)
                .await
        })
    }

    fn call_final_outcome_async_resuming_in_request<'a>(
        &'a self,
        ctx: &'a McpContext,
        request_cx: &'a Cx,
        arguments: serde_json::Value,
        resume_inputs: Option<&'a MrtrCompletedInputs>,
    ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
        Box::pin(async move {
            let transformed_args = match self.transform_arguments(arguments) {
                Ok(args) => args,
                Err(error) => return Outcome::Err(error),
            };
            self.parent
                .call_final_outcome_async_resuming_in_request(
                    ctx,
                    request_cx,
                    transformed_args,
                    resume_inputs,
                )
                .await
        })
    }
}

/// Builder for creating transformed tools.
pub struct TransformedToolBuilder {
    parent: BoxedToolHandler,
    name: Option<String>,
    description: Option<String>,
    arg_transforms: HashMap<String, ArgTransform>,
}

impl TransformedToolBuilder {
    /// Creates a new builder for the given parent tool.
    pub fn new(parent: BoxedToolHandler) -> Self {
        Self {
            parent,
            name: None,
            description: None,
            arg_transforms: HashMap::new(),
        }
    }

    /// Sets the new name for the transformed tool.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the new description for the transformed tool.
    #[must_use]
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Adds a transformation for the given argument.
    ///
    /// The `original_name` is the name of the argument in the parent tool.
    #[must_use]
    pub fn transform_arg(
        mut self,
        original_name: impl Into<String>,
        transform: ArgTransform,
    ) -> Self {
        self.arg_transforms.insert(original_name.into(), transform);
        self
    }

    /// Renames an argument.
    #[must_use]
    pub fn rename_arg(self, original_name: impl Into<String>, new_name: impl Into<String>) -> Self {
        self.transform_arg(original_name, ArgTransform::new().name(new_name))
    }

    /// Hides an argument and provides a default value.
    #[must_use]
    pub fn hide_arg(
        self,
        original_name: impl Into<String>,
        default: impl Into<serde_json::Value>,
    ) -> Self {
        self.transform_arg(original_name, ArgTransform::drop_with_default(default))
    }

    /// Builds the transformed tool.
    #[must_use]
    pub fn build(self) -> TransformedTool {
        let parent_def = self.parent.definition();

        // Build name mapping (new name -> original name)
        let mut name_mapping = HashMap::new();
        for (original, transform) in &self.arg_transforms {
            if let Some(new_name) = &transform.name {
                name_mapping.insert(new_name.clone(), original.clone());
            }
        }

        // Transform the tool definition
        let definition = self.build_definition(&parent_def);

        TransformedTool {
            parent: self.parent,
            definition,
            arg_transforms: self.arg_transforms,
            name_mapping,
        }
    }

    /// Builds the transformed tool definition.
    fn build_definition(&self, parent: &Tool) -> Tool {
        let name = self.name.clone().unwrap_or_else(|| parent.name.clone());
        let description = self
            .description
            .clone()
            .or_else(|| parent.description.clone());

        // Transform the input schema
        let input_schema = transform_input_schema(&self.arg_transforms, &parent.input_schema);

        Tool {
            name,
            description,
            input_schema,
            output_schema: parent.output_schema.clone(),
            icon: parent.icon.clone(),
            version: parent.version.clone(),
            tags: parent.tags.clone(),
            annotations: parent.annotations.clone(),
        }
    }
}

/// Rewrites a JSON Schema object according to argument rename/hide/default rules.
fn transform_input_schema(
    arg_transforms: &HashMap<String, ArgTransform>,
    original: &serde_json::Value,
) -> serde_json::Value {
    let mut schema = original.clone();

    let Some(obj) = schema.as_object_mut() else {
        return schema;
    };

    // Ensure properties and required exist as the shapes later mutation
    // expects. A parent tool may publish a non-object `properties` or
    // non-array `required`; replacing those malformed members keeps
    // `build()` from panicking on a definition the caller already owns.
    if !obj
        .get("properties")
        .is_some_and(serde_json::Value::is_object)
    {
        obj.insert(String::from("properties"), serde_json::json!({}));
    }
    if !obj.get("required").is_some_and(serde_json::Value::is_array) {
        obj.insert(String::from("required"), serde_json::json!([]));
    }

    // Track changes to apply
    // Pre-allocate based on transform count to avoid reallocations
    let capacity = arg_transforms.len();
    let mut props_to_remove: Vec<String> = Vec::with_capacity(capacity);
    let mut props_to_add: Vec<(String, serde_json::Value)> = Vec::with_capacity(capacity);
    let mut required_adds: Vec<String> = Vec::with_capacity(capacity);

    // First pass: collect property transformations
    {
        let Some(props) = obj.get("properties").and_then(serde_json::Value::as_object) else {
            return schema;
        };

        for (original_name, transform) in arg_transforms {
            if transform.hide {
                props_to_remove.push(original_name.clone());
                continue;
            }

            if let Some(prop_schema) = props.get(original_name).cloned() {
                let new_name = transform.name.as_ref().unwrap_or(original_name);
                let mut new_schema = prop_schema;

                // Select the replacement first. Explicit description/default
                // overrides belong to that schema, not to the discarded one.
                if let Some(type_schema) = &transform.type_schema {
                    new_schema = type_schema.clone();
                }

                // Apply description override
                if let (Some(desc), Some(schema_obj)) =
                    (&transform.description, new_schema.as_object_mut())
                {
                    schema_obj.insert(String::from("description"), serde_json::json!(desc));
                }

                // Apply default override
                if let (Some(default), Some(schema_obj)) =
                    (&transform.default, new_schema.as_object_mut())
                {
                    schema_obj.insert(String::from("default"), default.clone());
                }

                // Apply required override
                if transform.required == Some(true) {
                    required_adds.push(new_name.clone());
                }

                if new_name != original_name {
                    props_to_remove.push(original_name.clone());
                    props_to_add.push((new_name.clone(), new_schema));
                } else {
                    // Update in place
                    props_to_add.push((original_name.clone(), new_schema));
                }
            }
        }
    }

    // Apply property changes
    if let Some(props) = obj.get_mut("properties").and_then(|p| p.as_object_mut()) {
        for name in &props_to_remove {
            props.remove(name);
        }
        for (name, prop_schema) in props_to_add {
            props.insert(name, prop_schema);
        }
    }

    // Apply required array changes
    if let Some(required) = obj.get_mut("required").and_then(|r| r.as_array_mut()) {
        // Project each ORIGINAL requirement once. Sequential renames can
        // rename a previously rewritten entry again, losing requirements in
        // swaps/cycles. Likewise, removing by a published name can remove an
        // unrelated argument whose original name happens to be that alias.
        let original_required = std::mem::take(required);
        for value in original_required {
            let mapped = match value.as_str().and_then(|name| arg_transforms.get(name)) {
                Some(transform)
                    if transform.hide
                        || transform.required == Some(false)
                        || (transform.default.is_some() && transform.required != Some(true)) =>
                {
                    // Only actual adapter defaults permit omission. A schema's
                    // default annotation alone does not supply a runtime value.
                    continue;
                }
                Some(transform) => match &transform.name {
                    Some(name) => serde_json::Value::String(name.clone()),
                    None => value,
                },
                None => value,
            };
            if !required.contains(&mapped) {
                required.push(mapped);
            }
        }
        // Preserve existing requirement order, then add explicitly required
        // arguments deterministically rather than in HashMap iteration order.
        required_adds.sort_unstable();
        for name in required_adds {
            if !required.iter().any(|v| v.as_str() == Some(&name)) {
                required.push(serde_json::json!(name));
            }
        }
    }

    schema
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastmcp_core::block_on;
    use fastmcp_protocol::Content;
    use fastmcp_protocol::common_types::ContentBlock;

    #[test]
    fn regression_argument_name_swap_is_simultaneous() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("swap"))
            .rename_arg("q", "n")
            .rename_arg("n", "q")
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({"n": "query", "q": 7}))
                .unwrap(),
            serde_json::json!({"q": "query", "n": 7})
        );
    }

    #[test]
    fn regression_argument_rename_chain_preserves_both_inputs() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("chain"))
            .rename_arg("q", "n")
            .rename_arg("n", "limit")
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({
                "n": "query", "limit": 7, "q": "stale alias", "extra": false
            }))
            .unwrap(),
            serde_json::json!({"q": "query", "n": 7, "extra": false})
        );
    }

    #[test]
    fn regression_argument_three_cycle_preserves_all_values() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("cycle"))
            .rename_arg("a", "b")
            .rename_arg("b", "c")
            .rename_arg("c", "a")
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({"a": 3, "b": 1, "c": 2}))
                .unwrap(),
            serde_json::json!({"a": 1, "b": 2, "c": 3})
        );
    }

    #[test]
    fn regression_hidden_original_name_can_be_a_visible_alias() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("hidden"))
            .rename_arg("q", "n")
            .hide_arg("n", 10)
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({"n": "query", "q": "stale"}))
                .unwrap(),
            serde_json::json!({"q": "query", "n": 10})
        );
    }

    #[test]
    fn regression_hidden_alias_cannot_override_its_default() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("hidden"))
            .transform_arg("n", ArgTransform::new().name("limit").default(10).hide())
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({
                "q": "query", "n": 900, "limit": 800, "extra": 0
            }))
            .unwrap(),
            serde_json::json!({"q": "query", "n": 10, "extra": 0})
        );
    }

    #[test]
    fn regression_swap_preserves_present_null_and_falsy_values() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("values"))
            .transform_arg("q", ArgTransform::new().name("n").default("fallback"))
            .transform_arg("n", ArgTransform::new().name("q").default(99))
            .build();
        for value in [
            serde_json::Value::Null,
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!(""),
            serde_json::json!([]),
            serde_json::json!({}),
        ] {
            assert_eq!(
                tool.transform_arguments(serde_json::json!({"n": value.clone(), "q": 0}))
                    .unwrap(),
                serde_json::json!({"q": value, "n": 0})
            );
        }
    }

    #[test]
    fn regression_chain_defaults_only_replace_absent_published_values() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("defaults"))
            .transform_arg("q", ArgTransform::new().name("n").default("fallback"))
            .transform_arg("n", ArgTransform::new().name("limit").default(10))
            .build();
        assert_eq!(
            tool.transform_arguments(serde_json::json!({"n": "query"}))
                .unwrap(),
            serde_json::json!({"q": "query", "n": 10})
        );
        assert_eq!(
            tool.transform_arguments(serde_json::Value::Null).unwrap(),
            serde_json::json!({"q": "fallback", "n": 10})
        );
    }

    #[test]
    fn regression_hidden_argument_still_requires_a_server_default() {
        let tool = TransformedTool::from_tool(SearchToolFixture::new("missing-default"))
            .transform_arg("n", ArgTransform::new().name("limit").hide())
            .build();
        assert!(
            tool.transform_arguments(serde_json::json!({"n": 1, "limit": 2}))
                .is_err()
        );
        assert!(tool.transform_arguments(serde_json::json!([1, 2])).is_err());
    }

    fn regression_three_argument_schema(required: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "integer"},
                "c": {"type": "boolean"}
            },
            "required": required,
            "additionalProperties": false
        })
    }

    #[test]
    fn regression_schema_requirements_follow_a_swap_once() {
        let original = regression_three_argument_schema(serde_json::json!(["a", "b"]));
        let transforms = HashMap::from([
            ("a".to_owned(), ArgTransform::new().name("b")),
            ("b".to_owned(), ArgTransform::new().name("a")),
        ]);
        let actual = transform_input_schema(&transforms, &original);
        assert_eq!(actual["required"], serde_json::json!(["b", "a"]));
        assert_eq!(actual["properties"]["b"]["type"], "string");
        assert_eq!(actual["properties"]["a"]["type"], "integer");
        assert_eq!(actual["additionalProperties"], false);
        assert_eq!(original["required"], serde_json::json!(["a", "b"]));
    }

    #[test]
    fn regression_schema_requirements_follow_a_three_cycle_once() {
        let original = regression_three_argument_schema(serde_json::json!(["a", "b", "c"]));
        let transforms = HashMap::from([
            ("a".to_owned(), ArgTransform::new().name("b")),
            ("b".to_owned(), ArgTransform::new().name("c")),
            ("c".to_owned(), ArgTransform::new().name("a")),
        ]);
        let actual = transform_input_schema(&transforms, &original);
        assert_eq!(actual["required"], serde_json::json!(["b", "c", "a"]));
        assert_eq!(actual["properties"]["b"]["type"], "string");
        assert_eq!(actual["properties"]["c"]["type"], "integer");
        assert_eq!(actual["properties"]["a"]["type"], "boolean");
    }

    #[test]
    fn regression_optional_override_does_not_remove_another_alias_requirement() {
        let original = regression_three_argument_schema(serde_json::json!(["a", "b"]));
        let transforms = HashMap::from([
            ("a".to_owned(), ArgTransform::new().name("b").optional()),
            ("b".to_owned(), ArgTransform::new().name("a")),
        ]);
        assert_eq!(
            transform_input_schema(&transforms, &original)["required"],
            serde_json::json!(["a"])
        );
    }

    #[test]
    fn regression_hidden_original_does_not_remove_a_visible_alias_requirement() {
        let original = regression_three_argument_schema(serde_json::json!(["a", "b"]));
        let transforms = HashMap::from([
            ("a".to_owned(), ArgTransform::new().name("b")),
            ("b".to_owned(), ArgTransform::drop_with_default(10)),
        ]);
        let actual = transform_input_schema(&transforms, &original);
        assert_eq!(actual["required"], serde_json::json!(["b"]));
        assert_eq!(actual["properties"]["b"]["type"], "string");
    }

    #[test]
    fn regression_explicit_required_additions_are_stable_and_unique() {
        let original = regression_three_argument_schema(serde_json::json!(["c"]));
        let transforms = HashMap::from([
            ("a".to_owned(), ArgTransform::new().name("z").required()),
            ("b".to_owned(), ArgTransform::new().name("y").required()),
            ("c".to_owned(), ArgTransform::new().required()),
        ]);
        assert_eq!(
            transform_input_schema(&transforms, &original)["required"],
            serde_json::json!(["c", "y", "z"])
        );
    }

    #[test]
    fn regression_required_without_properties_is_renamed_consistently() {
        let original = serde_json::json!({"type": "object", "required": ["a"]});
        let transforms = HashMap::from([("a".to_owned(), ArgTransform::new().name("b"))]);
        assert_eq!(
            transform_input_schema(&transforms, &original)["required"],
            serde_json::json!(["b"])
        );
    }

    #[test]
    fn regression_schema_and_runtime_agree_on_overlapping_argument_names() {
        let mut parent = SearchToolFixture::new("roundtrip");
        parent.schema["required"] = serde_json::json!(["q", "n"]);
        let tool = TransformedTool::from_tool(parent)
            .rename_arg("q", "n")
            .rename_arg("n", "q")
            .build();
        let published = tool.definition().input_schema;
        assert_eq!(published["required"], serde_json::json!(["n", "q"]));
        assert_eq!(published["properties"]["n"]["type"], "string");
        assert_eq!(published["properties"]["q"]["type"], "integer");
        let input = serde_json::json!({"n": "query", "q": 4});
        assert_eq!(
            tool.transform_arguments(input).unwrap(),
            serde_json::json!({"q": "query", "n": 4})
        );
    }

    struct SearchToolFixture {
        name: String,
        description: Option<String>,
        schema: serde_json::Value,
    }

    impl SearchToolFixture {
        fn new(name: &str) -> Self {
            Self {
                name: name.to_string(),
                description: Some("Search tool".to_string()),
                schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "q": {
                            "type": "string",
                            "description": "Query"
                        },
                        "n": {
                            "type": "integer",
                            "description": "Limit"
                        }
                    },
                    "required": ["q"]
                }),
            }
        }
    }

    impl ToolHandler for SearchToolFixture {
        fn definition(&self) -> Tool {
            Tool {
                name: self.name.clone(),
                description: self.description.clone(),
                input_schema: self.schema.clone(),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            Ok(vec![Content::Text {
                text: format!("Search called with: {}", arguments),
            }])
        }
    }

    #[test]
    fn test_rename_tool() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .name("semantic_search")
            .description("Search semantically")
            .build();

        let def = transformed.definition();
        assert_eq!(def.name, "semantic_search");
        assert_eq!(def.description, Some("Search semantically".to_string()));
    }

    #[test]
    fn test_rename_arg() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();

        let def = transformed.definition();
        let props = def.input_schema["properties"].as_object().unwrap();

        // Original name should be gone
        assert!(!props.contains_key("q"));
        // New name should exist
        assert!(props.contains_key("query"));
    }

    #[test]
    fn test_hide_arg() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).hide_arg("n", 10).build();

        let def = transformed.definition();
        let props = def.input_schema["properties"].as_object().unwrap();

        // Hidden arg should not be in schema
        assert!(!props.contains_key("n"));
        // But q should still be there
        assert!(props.contains_key("q"));
    }

    #[test]
    fn test_transform_arguments() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .hide_arg("n", 10)
            .build();

        // Input uses new names
        let input = serde_json::json!({
            "query": "hello world"
        });

        // Transform should map back to original names and add defaults
        let result = transformed.transform_arguments(input).unwrap();
        let obj = result.as_object().unwrap();

        assert_eq!(obj.get("q").unwrap(), "hello world");
        assert_eq!(obj.get("n").unwrap(), 10);
    }

    #[test]
    fn test_arg_transform_builder() {
        let transform = ArgTransform::new()
            .name("search_query")
            .description("The search query string")
            .default_str("*")
            .required();

        assert_eq!(transform.name, Some("search_query".to_string()));
        assert_eq!(
            transform.description,
            Some("The search query string".to_string())
        );
        assert_eq!(transform.default, Some(serde_json::json!("*")));
        assert_eq!(transform.required, Some(true));
        assert!(!transform.hide);
    }

    // ── ArgTransform type helpers ─────────────────────────────────────

    #[test]
    fn arg_transform_default_int() {
        let t = ArgTransform::new().default_int(42);
        assert_eq!(t.default, Some(serde_json::json!(42)));
    }

    #[test]
    fn arg_transform_default_bool() {
        let t = ArgTransform::new().default_bool(true);
        assert_eq!(t.default, Some(serde_json::json!(true)));
    }

    #[test]
    fn arg_transform_type_schema() {
        let schema = serde_json::json!({"type": "number", "minimum": 0});
        let t = ArgTransform::new().type_schema(schema.clone());
        assert_eq!(t.type_schema, Some(schema));
    }

    #[test]
    fn arg_transform_drop_with_default() {
        let t = ArgTransform::drop_with_default("auto");
        assert!(t.hide);
        assert_eq!(t.default, Some(serde_json::json!("auto")));
    }

    #[test]
    fn arg_transform_hide_sets_flag() {
        let t = ArgTransform::new().hide();
        assert!(t.hide);
    }

    #[test]
    fn arg_transform_debug() {
        let t = ArgTransform::new().name("x");
        let debug = format!("{:?}", t);
        assert!(debug.contains("ArgTransform"));
    }

    #[test]
    fn arg_transform_clone() {
        let t = ArgTransform::new().name("x").default_int(5);
        let c = t.clone();
        assert_eq!(c.name, Some("x".to_string()));
        assert_eq!(c.default, Some(serde_json::json!(5)));
    }

    // ── TransformedTool accessors ─────────────────────────────────────

    #[test]
    fn transformed_tool_parent_definition() {
        let tool = SearchToolFixture::new("original");
        let transformed = TransformedTool::from_tool(tool).name("renamed").build();
        let parent_def = transformed.parent_definition();
        assert_eq!(parent_def.name, "original");
    }

    #[test]
    fn transformed_tool_arg_transforms_accessor() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();
        let transforms = transformed.arg_transforms();
        assert!(transforms.contains_key("q"));
    }

    #[test]
    fn transformed_tool_debug_format() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).name("dbg_tool").build();
        let debug = format!("{:?}", transformed);
        assert!(debug.contains("TransformedTool"));
        assert!(debug.contains("dbg_tool"));
    }

    #[test]
    fn transformed_tool_from_boxed() {
        let tool = Box::new(SearchToolFixture::new("boxed")) as BoxedToolHandler;
        let transformed = TransformedTool::from_boxed(tool).name("unboxed").build();
        assert_eq!(transformed.definition().name, "unboxed");
    }

    // ── transform_arguments edge cases ───────────────────────────────

    #[test]
    fn transform_arguments_null_treated_as_empty() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).hide_arg("n", 10).build();

        let result = transformed
            .transform_arguments(serde_json::Value::Null)
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("n").unwrap(), 10);
    }

    #[test]
    fn transform_arguments_non_object_returns_error() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).build();

        let result = transformed.transform_arguments(serde_json::json!("bad"));
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Arguments must be an object"));
    }

    #[test]
    fn transform_arguments_passthrough_unknown_args() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();

        let input = serde_json::json!({
            "query": "test",
            "extra": "value"
        });
        let result = transformed.transform_arguments(input).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("q").unwrap(), "test");
        assert_eq!(obj.get("extra").unwrap(), "value");
    }

    #[test]
    fn transform_arguments_rename_ignores_original_name_leftover() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();

        let result = transformed
            .transform_arguments(serde_json::json!({
                "query": "mapped",
                "q": "leftover"
            }))
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("q").unwrap(), "mapped");
        assert!(
            obj.len() == 1,
            "the unpublished original name must not leak beside the mapped value: {obj:?}"
        );
    }

    #[test]
    fn transform_arguments_hidden_without_default_errors() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new().hide())
            .build();

        let result = transformed.transform_arguments(serde_json::json!({}));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Hidden argument 'q' requires a default value")
        );
    }

    #[test]
    fn transform_arguments_hidden_default_ignores_caller_supplied_value() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).hide_arg("n", 10).build();

        let result = transformed
            .transform_arguments(serde_json::json!({"q": "hello", "n": 999}))
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("q").unwrap(), "hello");
        assert_eq!(
            obj.get("n").unwrap(),
            10,
            "a hidden argument must keep its server default"
        );
    }

    #[test]
    fn transform_arguments_hidden_renamed_strips_both_published_and_original_names() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg(
                "n",
                ArgTransform::new().name("limit").default_int(10).hide(),
            )
            .build();

        let result = transformed
            .transform_arguments(serde_json::json!({
                "q": "hello",
                "n": 1,
                "limit": 2
            }))
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("q").unwrap(), "hello");
        assert_eq!(obj.get("n").unwrap(), 10);
        assert!(
            obj.get("limit").is_none(),
            "the unpublished hidden name must not leak into parent arguments"
        );
    }

    #[test]
    fn transform_arguments_hidden_without_default_rejects_caller_value() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new().hide())
            .build();

        let result = transformed.transform_arguments(serde_json::json!({"q": "injected"}));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .message
                .contains("Hidden argument 'q' requires a default value")
        );
    }

    // ── ToolHandler impl ─────────────────────────────────────────────

    #[test]
    fn transformed_tool_call_delegates_with_mapped_args() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = transformed
            .call(&ctx, serde_json::json!({"query": "hello"}))
            .unwrap();
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn transformed_tool_call_with_invalid_args_returns_error() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool).build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = transformed.call(&ctx, serde_json::json!("string_not_object"));
        assert!(result.is_err());
    }

    // ── Builder keeps parent properties ──────────────────────────────

    #[test]
    fn builder_no_name_keeps_parent_name() {
        let tool = SearchToolFixture::new("original_name");
        let transformed = TransformedTool::from_tool(tool).build();
        assert_eq!(transformed.definition().name, "original_name");
    }

    #[test]
    fn builder_no_description_keeps_parent_description() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool).build();
        assert_eq!(
            transformed.definition().description,
            Some("Search tool".to_string())
        );
    }

    // ── Schema transform: description override ───────────────────────

    #[test]
    fn transform_schema_applies_description_override() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new().description("Full search query"))
            .build();

        let def = transformed.definition();
        let q_schema = &def.input_schema["properties"]["q"];
        assert_eq!(q_schema["description"], "Full search query");
    }

    // ── NotSet sentinel ──────────────────────────────────────────────

    #[test]
    fn not_set_debug() {
        let n = NotSet;
        let debug = format!("{:?}", n);
        assert!(debug.contains("NotSet"));
    }

    #[test]
    fn not_set_clone_copy() {
        let n = NotSet;
        let cloned = n.clone();
        let copied = n; // Copy
        let _ = (cloned, copied);
    }

    #[test]
    fn not_set_default() {
        let _ = NotSet;
    }

    // ── ArgTransform defaults ────────────────────────────────────────

    #[test]
    fn arg_transform_new_is_all_none() {
        let t = ArgTransform::new();
        assert!(t.name.is_none());
        assert!(t.description.is_none());
        assert!(t.default.is_none());
        assert!(!t.hide);
        assert!(t.required.is_none());
        assert!(t.type_schema.is_none());
    }

    #[test]
    fn arg_transform_default_trait() {
        let t = <ArgTransform as Default>::default();
        assert!(t.name.is_none());
        assert!(!t.hide);
    }

    // ── Schema transform: type override ──────────────────────────────

    #[test]
    fn transform_schema_replaces_malformed_properties_without_panicking() {
        struct MalformedSchemaTool;
        impl ToolHandler for MalformedSchemaTool {
            fn definition(&self) -> Tool {
                Tool {
                    name: "malformed".to_string(),
                    description: None,
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": true,
                        "required": "q"
                    }),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(MalformedSchemaTool)
            .hide_arg("secret", "server-owned")
            .build();
        assert_eq!(
            transformed.definition().input_schema["properties"],
            serde_json::json!({})
        );
        assert_eq!(
            transformed.definition().input_schema["required"],
            serde_json::json!([])
        );
    }

    #[test]
    fn transform_schema_applies_type_override() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg(
                "q",
                ArgTransform::new().type_schema(serde_json::json!({"type": "number"})),
            )
            .build();

        let def = transformed.definition();
        let q_schema = &def.input_schema["properties"]["q"];
        assert_eq!(q_schema["type"], "number");
    }

    // ── Schema transform: default value ──────────────────────────────

    #[test]
    fn transform_schema_applies_default_value() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new().default_int(25))
            .build();

        let def = transformed.definition();
        let n_schema = &def.input_schema["properties"]["n"];
        assert_eq!(n_schema["default"], 25);
    }

    // ── Schema rename updates required array ─────────────────────────

    #[test]
    fn transform_schema_rename_updates_required() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "query"));
        assert!(!required.iter().any(|v| v == "q"));
    }

    // ── Schema hide removes from required ────────────────────────────

    #[test]
    fn transform_schema_hide_removes_from_required() {
        // Make a tool where "q" is required, then hide it
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .hide_arg("q", "default-query")
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "q"));
    }

    // ── Schema required override modifies required array ──────────────

    #[test]
    fn transform_schema_required_makes_optional_arg_required() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new().required())
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "n"));
        assert!(required.iter().any(|v| v == "q"));
    }

    #[test]
    fn transform_schema_unspecified_required_preserves_optional_arg() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new())
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "n"));
        assert!(required.iter().any(|v| v == "q"));
    }

    #[test]
    fn transform_schema_renamed_and_required() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new().name("limit").required())
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "limit"));
        assert!(!required.iter().any(|v| v == "n"));
        assert!(required.iter().any(|v| v == "q"));
    }

    #[test]
    fn transform_schema_optional_makes_required_arg_optional() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new().optional())
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "q"));
        assert!(!required.iter().any(|v| v == "n"));
    }

    #[test]
    fn transform_schema_renamed_and_optional() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new().name("query").optional())
            .build();

        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(!required.iter().any(|v| v == "query"));
        assert!(!required.iter().any(|v| v == "q"));
    }

    // ── Combined transforms ──────────────────────────────────────────

    #[test]
    fn combined_rename_description_default() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg(
                "n",
                ArgTransform::new()
                    .name("limit")
                    .description("Max results")
                    .default_int(10),
            )
            .build();

        let def = transformed.definition();
        let props = def.input_schema["properties"].as_object().unwrap();
        assert!(!props.contains_key("n"));
        let limit = props.get("limit").unwrap();
        assert_eq!(limit["description"], "Max results");
        assert_eq!(limit["default"], 10);
    }

    // ── build_definition preserves parent metadata ───────────────────

    #[test]
    fn build_definition_preserves_parent_output_schema() {
        struct ToolWithOutputSchema;
        impl ToolHandler for ToolWithOutputSchema {
            fn definition(&self) -> Tool {
                Tool {
                    name: "parent".to_string(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: Some(serde_json::json!({"type": "string"})),
                    icon: None,
                    version: Some("2.0".to_string()),
                    tags: vec!["tag1".to_string()],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(ToolWithOutputSchema)
            .name("child")
            .build();
        let def = transformed.definition();
        assert_eq!(
            def.output_schema,
            Some(serde_json::json!({"type": "string"}))
        );
        assert_eq!(def.version, Some("2.0".to_string()));
        assert_eq!(def.tags, vec!["tag1".to_string()]);
    }

    // ── transform_schema with non-object schema ──────────────────────

    #[test]
    fn transform_schema_non_object_returned_as_is() {
        struct ArraySchemaTool;
        impl ToolHandler for ArraySchemaTool {
            fn definition(&self) -> Tool {
                Tool {
                    name: "arr".to_string(),
                    description: None,
                    input_schema: serde_json::json!("not an object"),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(ArraySchemaTool)
            .rename_arg("x", "y")
            .build();
        let def = transformed.definition();
        // Schema is returned as-is since it's not an object
        assert_eq!(def.input_schema, serde_json::json!("not an object"));
    }

    // ── Schema without properties or required ────────────────────────

    #[test]
    fn transform_schema_adds_properties_and_required_if_missing() {
        struct MinimalSchemaTool;
        impl ToolHandler for MinimalSchemaTool {
            fn definition(&self) -> Tool {
                Tool {
                    name: "min".to_string(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(MinimalSchemaTool).build();
        let def = transformed.definition();
        assert!(def.input_schema["properties"].is_object());
        assert!(def.input_schema["required"].is_array());
    }

    // ── TransformedTool call with hidden defaults ─────────────────────

    #[test]
    fn transformed_tool_call_injects_hidden_defaults() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .hide_arg("n", 5)
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = transformed
            .call(&ctx, serde_json::json!({"query": "test"}))
            .unwrap();
        // The result should contain the search output with mapped args
        assert_eq!(result.len(), 1);
        if let Content::Text { text } = &result[0] {
            assert!(text.contains("\"n\":5"));
            assert!(text.contains("\"q\":\"test\""));
        } else {
            panic!("expected text content");
        }
    }

    // ── transform_arg with no-op transform ───────────────────────────

    #[test]
    fn transform_arg_with_noop_keeps_original() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("q", ArgTransform::new())
            .build();

        let def = transformed.definition();
        let props = def.input_schema["properties"].as_object().unwrap();
        // q should still exist unchanged
        assert!(props.contains_key("q"));
    }

    // ── transform_arg for non-existent arg ───────────────────────────

    #[test]
    fn transform_arg_for_nonexistent_arg_is_ignored() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("nonexistent", "renamed")
            .build();

        let def = transformed.definition();
        let props = def.input_schema["properties"].as_object().unwrap();
        // Original args should be untouched
        assert!(props.contains_key("q"));
        assert!(props.contains_key("n"));
        // Renamed nonexistent shouldn't appear
        assert!(!props.contains_key("renamed"));
    }

    #[test]
    fn call_async_delegates_with_mapped_args() {
        use fastmcp_core::block_on;

        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .rename_arg("q", "query")
            .hide_arg("n", 7)
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = block_on(transformed.call_async(&ctx, serde_json::json!({"query": "async"})));
        let content = result.unwrap();
        assert_eq!(content.len(), 1);
        if let Content::Text { text } = &content[0] {
            assert!(text.contains("\"q\":\"async\""));
            assert!(text.contains("\"n\":7"));
        } else {
            panic!("expected text content");
        }
    }

    #[test]
    fn transform_arguments_no_value_no_default_not_hidden_skipped() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new().description("ignored desc"))
            .build();

        // Don't supply "n" at all - should skip it (no default, not hidden)
        let result = transformed
            .transform_arguments(serde_json::json!({"q": "hello"}))
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("q").unwrap(), "hello");
        assert!(
            obj.get("n").is_none(),
            "missing arg without default should be skipped"
        );
    }

    #[test]
    fn transform_arguments_default_used_without_hide() {
        let tool = SearchToolFixture::new("search");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg("n", ArgTransform::new().default_int(99))
            .build();

        // Don't supply "n" - default should kick in even though not hidden
        let result = transformed
            .transform_arguments(serde_json::json!({"q": "test"}))
            .unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(obj.get("n").unwrap(), 99);
    }

    #[test]
    fn build_definition_parent_no_description_returns_none() {
        struct NoDescTool;
        impl ToolHandler for NoDescTool {
            fn definition(&self) -> Tool {
                Tool {
                    name: "nodesc".to_string(),
                    description: None,
                    input_schema: serde_json::json!({"type": "object"}),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(NoDescTool).build();
        assert!(transformed.definition().description.is_none());
    }

    #[test]
    fn transform_schema_preserves_unrenamed_in_required() {
        // Create a tool with two required args; rename only one
        struct TwoReqTool;
        impl ToolHandler for TwoReqTool {
            fn definition(&self) -> Tool {
                Tool {
                    name: "two".to_string(),
                    description: None,
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "a": {"type": "string"},
                            "b": {"type": "string"}
                        },
                        "required": ["a", "b"]
                    }),
                    output_schema: None,
                    icon: None,
                    version: None,
                    tags: vec![],
                    annotations: None,
                }
            }
            fn call(&self, _ctx: &McpContext, _args: serde_json::Value) -> McpResult<Vec<Content>> {
                Ok(vec![])
            }
        }

        let transformed = TransformedTool::from_tool(TwoReqTool)
            .rename_arg("a", "alpha")
            .build();
        let def = transformed.definition();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(
            required.iter().any(|v| v == "alpha"),
            "renamed arg in required"
        );
        assert!(
            required.iter().any(|v| v == "b"),
            "unrenamed arg still in required"
        );
        assert!(
            !required.iter().any(|v| v == "a"),
            "old name removed from required"
        );
    }

    #[test]
    fn type_schema_replaces_entire_property() {
        let tool = SearchToolFixture::new("s");
        let transformed = TransformedTool::from_tool(tool)
            .transform_arg(
                "q",
                ArgTransform::new()
                    .type_schema(serde_json::json!({"type": "array", "items": {"type": "string"}})),
            )
            .build();

        let def = transformed.definition();
        let q_schema = &def.input_schema["properties"]["q"];
        // Should have the new type, not the old "string"
        assert_eq!(q_schema["type"], "array");
        assert!(q_schema["items"].is_object());
        // The old description should NOT be present (type_schema replaces entirely)
        assert!(q_schema.get("description").is_none());
    }

    struct FinalAwareParent {
        recorded: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
        resume_hook_invoked: std::sync::Arc<std::sync::Mutex<bool>>,
    }

    impl FinalAwareParent {
        fn new() -> Self {
            Self {
                recorded: std::sync::Arc::new(std::sync::Mutex::new(None)),
                resume_hook_invoked: std::sync::Arc::new(std::sync::Mutex::new(false)),
            }
        }

        fn search_schema() -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {
                    "q": { "type": "string" },
                    "n": { "type": "integer" }
                },
                "required": ["q"]
            })
        }
    }

    impl ToolHandler for FinalAwareParent {
        fn definition(&self) -> Tool {
            Tool {
                name: "search".to_string(),
                description: Some("Search tool".to_string()),
                input_schema: Self::search_schema(),
                output_schema: None,
                icon: None,
                version: None,
                tags: vec![],
                annotations: None,
            }
        }

        fn timeout(&self) -> Option<Duration> {
            Some(Duration::from_secs(3))
        }

        fn declares_final_mrtr(&self) -> bool {
            true
        }

        fn final_title(&self) -> Option<&str> {
            Some("Search")
        }

        fn final_definition(&self) -> Option<FinalTool> {
            Some(FinalTool {
                name: "search".to_string(),
                title: Some("Search".to_string()),
                description: Some("Search tool".to_string()),
                icons: None,
                input_schema: Self::search_schema(),
                output_schema: None,
                annotations: None,
                meta: None,
            })
        }

        fn call(&self, _ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            *self
                .recorded
                .lock()
                .expect("final-aware parent argument log is not poisoned") = Some(arguments);
            Ok(vec![Content::text("legacy")])
        }

        fn call_final(
            &self,
            _ctx: &McpContext,
            arguments: serde_json::Value,
        ) -> McpResult<CompleteResult<FinalCallToolResult>> {
            *self
                .recorded
                .lock()
                .expect("final-aware parent argument log is not poisoned") = Some(arguments);
            crate::handler::promote_legacy_tool_content(vec![Content::text("final")])
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            _request_cx: &'a Cx,
            arguments: serde_json::Value,
            _resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            Box::pin(async move {
                *self
                    .resume_hook_invoked
                    .lock()
                    .expect("final-aware parent resume log is not poisoned") = true;
                match self.call_final(ctx, arguments) {
                    Ok(result) => Outcome::Ok(FinalToolOutcome::Complete(result)),
                    Err(error) => Outcome::Err(error),
                }
            })
        }
    }

    #[test]
    fn transformed_tool_forwards_timeout_and_final_definition() {
        let transformed = TransformedTool::from_tool(FinalAwareParent::new())
            .name("semantic_search")
            .rename_arg("q", "query")
            .hide_arg("n", 10)
            .build();

        assert_eq!(transformed.timeout(), Some(Duration::from_secs(3)));
        assert!(transformed.declares_final_mrtr());
        assert_eq!(transformed.final_title(), Some("Search"));

        let final_definition = transformed
            .final_definition()
            .expect("parent final definition must survive the transform wrapper");
        assert_eq!(final_definition.name, "semantic_search");
        assert_eq!(final_definition.title.as_deref(), Some("Search"));
        let properties = final_definition.input_schema["properties"]
            .as_object()
            .expect("final input schema keeps object properties");
        assert!(
            properties.contains_key("query"),
            "final catalog must publish the renamed argument: {properties:?}"
        );
        assert!(
            !properties.contains_key("q"),
            "final catalog must not keep the unpublished original name: {properties:?}"
        );
        assert!(
            !properties.contains_key("n"),
            "final catalog must hide the dropped argument: {properties:?}"
        );
    }

    #[test]
    fn transformed_tool_call_final_uses_parent_final_hook_with_mapped_args() {
        let parent = FinalAwareParent::new();
        let recorded = std::sync::Arc::clone(&parent.recorded);
        let transformed = TransformedTool::from_tool(parent)
            .name("semantic_search")
            .rename_arg("q", "query")
            .hide_arg("n", 10)
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let result = transformed
            .call_final(&ctx, serde_json::json!({"query": "test"}))
            .expect("transformed final call must succeed");
        match result.payload.content.as_slice() {
            [ContentBlock::Text { text, .. }] => {
                assert_eq!(
                    text, "final",
                    "TransformedTool must invoke the parent final hook instead of promoting call()"
                );
            }
            other => panic!("expected one final text block, got {other:?}"),
        }
        let recorded = recorded
            .lock()
            .expect("final-aware parent argument log is not poisoned")
            .clone()
            .expect("parent final hook must observe mapped arguments");
        assert_eq!(recorded["q"], "test");
        assert_eq!(recorded["n"], 10);
        assert!(
            recorded.get("query").is_none(),
            "parent must see original names, not the published aliases: {recorded}"
        );
    }

    #[test]
    fn transformed_tool_call_final_does_not_promote_legacy_call() {
        let parent = FinalAwareParent::new();
        let transformed = TransformedTool::from_tool(parent)
            .rename_arg("q", "query")
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx, 1);
        let legacy = transformed
            .call(&ctx, serde_json::json!({"query": "legacy-path"}))
            .expect("legacy call still works");
        match &legacy[0] {
            Content::Text { text, .. } => assert_eq!(text, "legacy"),
            other => panic!("expected legacy text content, got {other:?}"),
        }

        let final_result = transformed
            .call_final(&ctx, serde_json::json!({"query": "final-path"}))
            .expect("final call still works");
        match final_result.payload.content.as_slice() {
            [ContentBlock::Text { text, .. }] => {
                assert_eq!(text, "final");
                assert_ne!(
                    text, "legacy",
                    "the planted dimension is which parent hook ran"
                );
            }
            other => panic!("expected one final text block, got {other:?}"),
        }
    }

    #[test]
    fn transformed_tool_forwards_mrtr_resume_hook() {
        let parent = FinalAwareParent::new();
        let resume_hook_invoked = std::sync::Arc::clone(&parent.resume_hook_invoked);
        let transformed = TransformedTool::from_tool(parent)
            .rename_arg("q", "query")
            .build();

        let cx = asupersync::Cx::for_testing();
        let ctx = McpContext::new(cx.clone(), 1);
        let outcome = block_on(transformed.call_final_outcome_async_resuming_in_request(
            &ctx,
            &cx,
            serde_json::json!({"query": "resume"}),
            None,
        ));
        match outcome {
            Outcome::Ok(FinalToolOutcome::Complete(result)) => {
                match result.payload.content.as_slice() {
                    [ContentBlock::Text { text, .. }] => {
                        assert_eq!(text, "final");
                    }
                    other => panic!("expected one final text block, got {other:?}"),
                }
            }
            other => panic!(
                "expected a complete final outcome, got {}",
                match other {
                    Outcome::Ok(_) => "Ok(non-complete)",
                    Outcome::Err(_) => "Err",
                    Outcome::Cancelled(_) => "Cancelled",
                    Outcome::Panicked(_) => "Panicked",
                }
            ),
        }
        assert!(
            *resume_hook_invoked
                .lock()
                .expect("final-aware parent resume log is not poisoned"),
            "TransformedTool must call the parent MRTR resume hook instead of dropping resume inputs"
        );
    }

    #[test]
    fn runtime_defaults_make_renamed_arguments_optional_in_both_catalogs() {
        for required in [false, true] {
            let mut transform = ArgTransform::new().name("query").default_str("fallback");
            if required {
                transform = transform.required();
            }
            let tool = TransformedTool::from_tool(FinalAwareParent::new())
                .transform_arg("q", transform)
                .build();
            for schema in [
                tool.definition().input_schema,
                tool.final_definition().unwrap().input_schema,
            ] {
                assert_eq!(schema["properties"]["query"]["default"], "fallback");
                let admitted = fastmcp_protocol::admit_final_schema(schema).unwrap();
                assert_eq!(admitted.validate(&serde_json::json!({})).is_err(), required);
                assert!(admitted.validate(&serde_json::json!({"query": ""})).is_ok());
                assert!(admitted.validate(&serde_json::json!({"query": null})).is_err());
            }
        }
    }

    #[test]
    fn falsy_runtime_defaults_are_not_confused_with_absent_defaults() {
        for default in [
            serde_json::json!(null),
            serde_json::json!(false),
            serde_json::json!(0),
            serde_json::json!(""),
        ] {
            let mut parent = SearchToolFixture::new("defaults");
            parent.schema = serde_json::json!({
                "type": "object",
                "properties": {"value": {}},
                "required": ["value"]
            });
            let tool = TransformedTool::from_tool(parent)
                .transform_arg("value", ArgTransform::new().default(default.clone()))
                .build();
            let schema =
                fastmcp_protocol::admit_final_schema(tool.definition().input_schema).unwrap();
            assert!(schema.validate(&serde_json::json!({})).is_ok());
            assert_eq!(
                tool.transform_arguments(serde_json::json!({})).unwrap(),
                serde_json::json!({"value": default})
            );
        }
    }

    #[test]
    fn schema_default_annotations_alone_do_not_relax_required_arguments() {
        let mut parent = SearchToolFixture::new("annotation");
        parent.schema["properties"]["q"]["default"] = serde_json::json!("annotation-only");
        let tool = TransformedTool::from_tool(parent)
            .rename_arg("q", "query")
            .build();
        let schema = fastmcp_protocol::admit_final_schema(tool.definition().input_schema).unwrap();
        assert!(schema.validate(&serde_json::json!({})).is_err());
        assert!(
            schema
                .validate(&serde_json::json!({"query": "explicit"}))
                .is_ok()
        );
        assert_eq!(
            tool.transform_arguments(serde_json::json!({})).unwrap(),
            serde_json::json!({})
        );
    }

    #[test]
    fn explicit_description_and_default_override_the_replacement_schema() {
        for explicit_description in [false, true] {
            let mut transform = ArgTransform::new()
                .name("query")
                .type_schema(serde_json::json!({
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Replacement description",
                    "default": ["replacement"]
                }))
                .default(serde_json::json!(["configured"]));
            if explicit_description {
                transform = transform.description("Explicit description");
            }
            let tool = TransformedTool::from_tool(FinalAwareParent::new())
                .transform_arg("q", transform)
                .build();
            for schema in [
                tool.definition().input_schema,
                tool.final_definition().unwrap().input_schema,
            ] {
                let property = &schema["properties"]["query"];
                assert_eq!(
                    property["description"],
                    if explicit_description {
                        "Explicit description"
                    } else {
                        "Replacement description"
                    }
                );
                assert_eq!(property["default"], serde_json::json!(["configured"]));
                assert_eq!(property["type"], "array");
                let admitted = fastmcp_protocol::admit_final_schema(schema).unwrap();
                assert!(
                    admitted
                        .validate(&serde_json::json!({"query": ["item"]}))
                        .is_ok()
                );
                assert!(
                    admitted
                        .validate(&serde_json::json!({"query": "item"}))
                        .is_err()
                );
            }
        }
    }

    struct AsyncDefaultParent(FinalAwareParent);

    impl ToolHandler for AsyncDefaultParent {
        fn definition(&self) -> Tool {
            self.0.definition()
        }

        fn final_definition(&self) -> Option<FinalTool> {
            self.0.final_definition()
        }

        fn execution_mode(&self) -> crate::ToolExecutionMode {
            crate::ToolExecutionMode::Async
        }

        fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            self.0.call(ctx, arguments)
        }

        fn call_final_outcome_async_resuming_in_request<'a>(
            &'a self,
            ctx: &'a McpContext,
            request_cx: &'a Cx,
            arguments: serde_json::Value,
            resume_inputs: Option<&'a MrtrCompletedInputs>,
        ) -> BoxFuture<'a, McpOutcome<FinalToolOutcome>> {
            self.0.call_final_outcome_async_resuming_in_request(
                ctx,
                request_cx,
                arguments,
                resume_inputs,
            )
        }
    }

    #[test]
    fn router_dispatch_applies_defaults_but_preserves_explicit_required_and_type_checks() {
        use std::sync::Arc;

        use fastmcp_protocol::{FINAL_PROTOCOL_VERSION, JsonRpcRequest};

        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .blocking_threads(0, 0)
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            for required in [false, true] {
                let parent = FinalAwareParent::new();
                let recorded = Arc::clone(&parent.recorded);
                let mut transform = ArgTransform::new().name("query").default_str("fallback");
                if required {
                    transform = transform.required();
                }
                let tool = TransformedTool::from_tool(AsyncDefaultParent(parent))
                    .transform_arg("q", transform)
                    .build();
                let mut router = crate::Router::new();
                router.add_tool(tool).unwrap();
                let router = Arc::new(router);
                for (id, arguments, rejected, expected) in [
                    (
                        1_i64,
                        serde_json::json!({}),
                        required,
                        serde_json::json!({"q": "fallback"}),
                    ),
                    (
                        2_i64,
                        serde_json::json!({"query": ""}),
                        false,
                        serde_json::json!({"q": ""}),
                    ),
                    (
                        3_i64,
                        serde_json::json!({"query": null}),
                        true,
                        serde_json::json!(null),
                    ),
                ] {
                    assert!(recorded.lock().unwrap().is_none());
                    let request = JsonRpcRequest::new(
                        "tools/call",
                        Some(serde_json::json!({
                            "name": "search",
                            "arguments": arguments,
                            "_meta": {
                                "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                                "io.modelcontextprotocol/clientCapabilities": {}
                            }
                        })),
                        id,
                    );
                    let context = McpContext::with_state(
                        cx.clone(),
                        id as u64,
                        fastmcp_core::SessionState::new(),
                    );
                    let response = Arc::clone(&router)
                        .dispatch_stateless_owned(context, request)
                        .await
                        .unwrap();
                    assert_eq!(response["resultType"], "complete");
                    // Successful replies omit isError. Decode its typed wire
                    // default instead of requiring an explicit false member.
                    let payload: FinalCallToolResult =
                        serde_json::from_value(response.clone()).unwrap();
                    assert_eq!(payload.is_error, rejected);
                    let observed = recorded.lock().unwrap().take();
                    if rejected {
                        assert!(observed.is_none(), "rejected input must not reach the parent");
                    } else {
                        assert_eq!(observed, Some(expected));
                        assert_eq!(response["content"][0]["text"], "final");
                    }
                }
            }
        });
        assert!(runtime.shutdown_timeout(Duration::from_secs(1)));
    }
}
