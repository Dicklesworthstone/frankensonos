//! Caller-provisioned, bounded collections of Draft 2020-12 schema resources.
//!
//! A URI is an identifier here, never permission to fetch a URL or open a file.
//! Applications supply every resource explicitly. Compilation bundles those
//! resources under `$defs` and delegates admission and validation to the same
//! engine used by MCP tool schemas. References and resource boundaries are not
//! rewritten, so local pointers, anchors and dynamic references keep their scope.
//!
//! `insert` stages bounded candidates, including forward references; it is not
//! schema admission. `compile` admits the entire supplied collection, rejects
//! unresolved references and ambiguous identifiers, and returns an independent
//! `AdmittedSchema`. Adding a candidate later cannot change an earlier compiled
//! validator. Use a separate registry for each trust/policy domain.
//!
//! Registration takes an absolute canonical resource identity. A document with
//! a root `$id` must use that exact identity; retrieval aliases are not inferred.
//! Anonymous documents receive that `$id`. Boolean resources are represented by
//! an identified `allOf` wrapper, preserving their validation meaning. Original
//! references, annotations, assertions and existing `$defs` members are retained.
//! This supplies local resource composition, not full Draft 2020-12 conformance
//! or support for vocabularies the shared validator does not implement.

use std::collections::BTreeMap;
use std::fmt;
use std::io::{self, Write};

use fastmcp_core::AbsoluteUri;
use serde_json::{Map, Value};

use crate::schema::{
    AdmittedSchema, MAX_SCHEMA_ADMISSION_NODES, MAX_SCHEMA_ASSERTION_STRING_BYTES,
    MAX_SCHEMA_VALIDATION_DEPTH, SchemaAdmissionError, admit_final_schema,
};

pub const MAX_SCHEMA_REGISTRY_RESOURCES: usize = 128;
pub const MAX_SCHEMA_REGISTRY_RESOURCE_BYTES: usize = 256 * 1024;
pub const MAX_SCHEMA_REGISTRY_BYTES: usize = 2 * 1024 * 1024;
const MAX_REGISTRY_VALUE_NODES: usize = 65_536;

/// Bounds candidate retention as well as the resulting compound document.
/// The shared engine independently enforces its admission and evaluation limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SchemaRegistryLimits {
    resources: usize,
    resource_bytes: usize,
    total_bytes: usize,
}

impl Default for SchemaRegistryLimits {
    fn default() -> Self {
        Self {
            resources: MAX_SCHEMA_REGISTRY_RESOURCES,
            resource_bytes: MAX_SCHEMA_REGISTRY_RESOURCE_BYTES,
            total_bytes: MAX_SCHEMA_REGISTRY_BYTES,
        }
    }
}

impl SchemaRegistryLimits {
    pub fn new(resources: usize, resource_bytes: usize, total_bytes: usize) -> Result<Self, SchemaRegistryError> {
        if !(1..=MAX_SCHEMA_REGISTRY_RESOURCES).contains(&resources)
            || !(1..=MAX_SCHEMA_REGISTRY_RESOURCE_BYTES).contains(&resource_bytes)
            || !(1..=MAX_SCHEMA_REGISTRY_BYTES).contains(&total_bytes)
            || resource_bytes > total_bytes
        {
            return Err(SchemaRegistryError::InvalidLimits);
        }
        Ok(Self { resources, resource_bytes, total_bytes })
    }
}

/// Errors retain no resource document or URI. Schema admission errors retain
/// the shared validator's bounded path and fixed reason, not instance data.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchemaRegistryError {
    InvalidLimits,
    InvalidResourceId,
    IdentityMismatch,
    DuplicateResource,
    UnknownResource,
    InvalidSchema,
    ResourceLimit,
    ResourceTooLarge,
    TotalByteLimit,
    NestingLimit,
    NodeLimit,
    Admission(SchemaAdmissionError),
}

impl fmt::Display for SchemaRegistryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidLimits => f.write_str("invalid schema registry limits"),
            Self::InvalidResourceId => f.write_str("schema resource identity must be a bounded absolute URI without a fragment"),
            Self::IdentityMismatch => f.write_str("schema root identifier differs from its registered identity"),
            Self::DuplicateResource => f.write_str("schema resource identity is already registered"),
            Self::UnknownResource => f.write_str("schema resource is not registered"),
            Self::InvalidSchema => f.write_str("schema resource has an invalid document shape"),
            Self::ResourceLimit => f.write_str("schema registry resource limit exceeded"),
            Self::ResourceTooLarge => f.write_str("schema resource byte limit exceeded"),
            Self::TotalByteLimit => f.write_str("schema registry total byte limit exceeded"),
            Self::NestingLimit => f.write_str("schema registry candidate nesting limit exceeded"),
            Self::NodeLimit => f.write_str("schema registry candidate node limit exceeded"),
            Self::Admission(error) => error.fmt(f),
        }
    }
}
impl std::error::Error for SchemaRegistryError {}
impl From<SchemaAdmissionError> for SchemaRegistryError {
    fn from(error: SchemaAdmissionError) -> Self { Self::Admission(error) }
}

/// One explicit trust domain's candidates. There is no global registry, cache,
/// loader, network callback, filesystem access, or silently replaceable identity.
pub struct SchemaResourceRegistry {
    resources: BTreeMap<String, Value>,
    retained_bytes: usize,
    value_nodes: usize,
    limits: SchemaRegistryLimits,
}

impl Default for SchemaResourceRegistry {
    fn default() -> Self { Self::new(SchemaRegistryLimits::default()) }
}

impl fmt::Debug for SchemaResourceRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SchemaResourceRegistry")
            .field("resources", &self.resources.len())
            .field("retained_bytes", &self.retained_bytes)
            .finish_non_exhaustive()
    }
}

impl SchemaResourceRegistry {
    pub fn new(limits: SchemaRegistryLimits) -> Self {
        Self { resources: BTreeMap::new(), retained_bytes: 0, value_nodes: 0, limits }
    }

    pub fn len(&self) -> usize { self.resources.len() }
    pub fn is_empty(&self) -> bool { self.resources.is_empty() }
    /// Serialized candidate bytes plus their separately retained lookup keys.
    /// Map/allocation overhead is separately bounded by resource and node counts.
    pub fn retained_bytes(&self) -> usize { self.retained_bytes }

    /// Stage one resource atomically. Forward and cyclic references may be
    /// registered in any order; compilation, not insertion, resolves them.
    /// A rejected candidate changes neither registry contents nor accounting.
    pub fn insert(&mut self, identity: &str, schema: Value) -> Result<(), SchemaRegistryError> {
        let uri = AbsoluteUri::parse_with_max_bytes(identity, MAX_SCHEMA_ASSERTION_STRING_BYTES)
            .map_err(|_| SchemaRegistryError::InvalidResourceId)?;
        if uri.fragment().is_some() { return Err(SchemaRegistryError::InvalidResourceId); }
        if self.resources.contains_key(identity) { return Err(SchemaRegistryError::DuplicateResource); }
        if self.resources.len() >= self.limits.resources { return Err(SchemaRegistryError::ResourceLimit); }
        if !schema.is_object() && !schema.is_boolean() { return Err(SchemaRegistryError::InvalidSchema); }

        // Bound all JSON values, including annotations that the semantic schema
        // walker need not visit. Do this before wrapping, retaining or cloning.
        let mut nodes = 0;
        count_values(&schema, 0, &mut nodes)?;
        let mut schema = match schema {
            Value::Object(mut object) => {
                if object.get("$id").is_some_and(|id| id.as_str() != Some(identity)) {
                    return Err(SchemaRegistryError::IdentityMismatch);
                }
                if !object.contains_key("$id") {
                    object.insert("$id".to_owned(), Value::String(identity.to_owned()));
                    nodes += 1;
                }
                Value::Object(object)
            }
            boolean => {
                nodes += 3;
                let mut object = Map::new();
                object.insert("$id".to_owned(), Value::String(identity.to_owned()));
                object.insert("allOf".to_owned(), Value::Array(vec![boolean]));
                Value::Object(object)
            }
        };
        // Do not let a malformed definitions map acquire a different meaning
        // later when compilation adds its separate, identified resources.
        if schema.get("$defs").is_some_and(|defs| !defs.is_object()) {
            return Err(SchemaRegistryError::InvalidSchema);
        }
        let bytes = encoded_size(&schema, self.limits.resource_bytes)
            .ok_or(SchemaRegistryError::ResourceTooLarge)?;
        let retained = self.retained_bytes.checked_add(identity.len())
            .and_then(|total| total.checked_add(bytes))
            .filter(|total| *total <= self.limits.total_bytes)
            .ok_or(SchemaRegistryError::TotalByteLimit)?;
        let total_nodes = self.value_nodes.checked_add(nodes)
            .filter(|total| *total <= MAX_REGISTRY_VALUE_NODES)
            .ok_or(SchemaRegistryError::NodeLimit)?;
        // All validation and accounting precede the single retained mutation.
        self.resources.insert(identity.to_owned(), std::mem::take(&mut schema));
        self.retained_bytes = retained;
        self.value_nodes = total_nodes;
        Ok(())
    }

    /// Compile a selected root together with every explicitly staged resource.
    /// The result is usable by existing `AdmittedSchema` consumers and its
    /// `schema()` can be shipped as a self-contained compound schema document.
    ///
    /// Existing root JSON Pointers remain valid: the root is not moved below a
    /// synthetic `$ref`. Other documents retain their own absolute `$id`, so
    /// their relative references and local anchors do not acquire the root's
    /// base. Unreferenced staged documents are also admitted; construct a
    /// separate registry when another document belongs to a different policy.
    pub fn compile(&self, identity: &str) -> Result<AdmittedSchema, SchemaRegistryError> {
        let mut root = self.resources.get(identity).ok_or(SchemaRegistryError::UnknownResource)?.clone();
        let object = root.as_object_mut().ok_or(SchemaRegistryError::InvalidSchema)?;
        if self.resources.len() > 1 {
            let definitions = object.entry("$defs".to_owned()).or_insert_with(|| Value::Object(Map::new()))
                .as_object_mut().ok_or(SchemaRegistryError::InvalidSchema)?;
            let mut index = 0usize;
            for (other, resource) in &self.resources {
                if other == identity { continue; }
                // Preserve even a caller's preexisting generated-looking name.
                // Each collision consumes one existing member; the candidate
                // node/resource bounds also bound this loop.
                let name = loop {
                    let name = format!("_fastmcp_registry_{index}");
                    index += 1;
                    if !definitions.contains_key(&name) { break name; }
                };
                definitions.insert(name, resource.clone());
            }
        }
        encoded_size(&root, self.limits.total_bytes).ok_or(SchemaRegistryError::TotalByteLimit)?;
        // Reuse the engine's exact resource/anchor-uniqueness checks, schema
        // admission, dialect policy, reference resolution and work ceilings.
        Ok(admit_final_schema(root)?)
    }
}

fn count_values(value: &Value, depth: usize, count: &mut usize) -> Result<(), SchemaRegistryError> {
    if depth >= MAX_SCHEMA_VALIDATION_DEPTH { return Err(SchemaRegistryError::NestingLimit); }
    *count += 1;
    if *count > MAX_SCHEMA_ADMISSION_NODES { return Err(SchemaRegistryError::NodeLimit); }
    match value {
        Value::Array(values) => {
            for value in values { count_values(value, depth + 1, count)?; }
        }
        Value::Object(values) => {
            for value in values.values() { count_values(value, depth + 1, count)?; }
        }
        _ => {}
    }
    Ok(())
}

struct SizeWriter { size: usize, maximum: usize }
impl Write for SizeWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.size = self.size.checked_add(bytes.len()).filter(|size| *size <= self.maximum)
            .ok_or_else(|| io::Error::other("schema byte limit"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}
fn encoded_size(value: &Value, maximum: usize) -> Option<usize> {
    let mut writer = SizeWriter { size: 0, maximum };
    serde_json::to_writer(&mut writer, value).ok()?;
    Some(writer.size)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const ROOT: &str = "https://schemas.example/tool";
    const VALUE: &str = "https://schemas.example/value";

    fn registry() -> SchemaResourceRegistry {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({
            "type":"object", "properties":{"value":{"$ref":"value"}},
            "required":["value"], "additionalProperties":false,
        })).unwrap();
        registry.insert(VALUE, json!({"type":"integer", "minimum":1})).unwrap();
        registry
    }

    #[test]
    fn separately_provisioned_resources_validate_real_tool_arguments() {
        let registry = registry();
        let schema = registry.compile(ROOT).unwrap();
        assert!(schema.validate(&json!({"value":2})).is_ok());
        assert!(schema.validate(&json!({"value":0})).is_err());
        assert!(schema.validate(&json!({"value":"2"})).is_err());
        assert!(schema.validate(&json!({"value":2,"extra":true})).is_err());
        assert_eq!(schema.schema()["properties"]["value"]["$ref"], "value");
        // Re-admission uses the ordinary API: no registry or loader is needed
        // by a consumer receiving the bundled tool schema.
        let transmitted = admit_final_schema(schema.schema().clone()).unwrap();
        assert!(transmitted.validate(&json!({"value":2})).is_ok());
        assert!(transmitted.validate(&json!({"value":0})).is_err());
    }

    #[test]
    fn forward_references_are_not_silently_accepted_when_unresolved() {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({"$ref":VALUE})).unwrap();
        let before = (registry.len(), registry.retained_bytes());
        assert!(registry.compile(ROOT).is_err());
        assert_eq!((registry.len(), registry.retained_bytes()), before);
        registry.insert(VALUE, json!({"const":"ready"})).unwrap();
        let admitted = registry.compile(ROOT).unwrap();
        assert!(admitted.validate(&json!("ready")).is_ok());
        assert!(admitted.validate(&json!("not-ready")).is_err());
    }

    #[test]
    fn registry_does_not_fetch_files_or_network_references() {
        for target in ["https://unregistered.example/schema", "file:///private/schema.json"] {
            let mut registry = SchemaResourceRegistry::default();
            registry.insert(ROOT, json!({"$ref":target})).unwrap();
            assert!(registry.compile(ROOT).is_err());
            assert_eq!(registry.len(), 1);
        }
    }

    #[test]
    fn existing_root_pointers_and_generated_looking_definitions_survive() {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({
            "$defs":{"_fastmcp_registry_0":{"const":7}, "a/b":{"const":9}},
            "allOf":[{"$ref":"#/$defs/_fastmcp_registry_0"},{"$ref":VALUE}],
        })).unwrap();
        registry.insert(VALUE, json!({"type":"integer"})).unwrap();
        let schema = registry.compile(ROOT).unwrap();
        assert_eq!(schema.schema()["$defs"]["_fastmcp_registry_0"], json!({"const":7}));
        assert_eq!(schema.schema()["$defs"]["a/b"], json!({"const":9}));
        assert!(schema.validate(&json!(7)).is_ok());
        assert!(schema.validate(&json!(8)).is_err());
    }

    #[test]
    fn nested_identifiers_keep_their_base_and_anchor_scope() {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({"$ref":"nested/item#value"})).unwrap();
        registry.insert(VALUE, json!({"$defs":{"nested":{
            "$id":"nested/item", "$anchor":"value", "type":"string", "minLength":2,
        }}})).unwrap();
        let schema = registry.compile(ROOT).unwrap();
        assert!(schema.validate(&json!("yes")).is_ok());
        assert!(schema.validate(&json!("x")).is_err());
    }

    #[test]
    fn recursive_resources_do_not_require_reference_expansion() {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({
            "type":"object", "properties":{"next":{"$ref":VALUE}}, "additionalProperties":false,
        })).unwrap();
        registry.insert(VALUE, json!({
            "type":"object", "properties":{"next":{"$ref":ROOT}}, "additionalProperties":false,
        })).unwrap();
        let schema = registry.compile(ROOT).unwrap();
        assert!(schema.validate(&json!({"next":{"next":{}}})).is_ok());
        assert!(schema.validate(&json!({"next":{"next":3}})).is_err());
    }

    #[test]
    fn boolean_resources_keep_true_and_false_semantics() {
        for allowed in [true, false] {
            let mut registry = SchemaResourceRegistry::default();
            registry.insert(ROOT, json!({"$ref":VALUE})).unwrap();
            registry.insert(VALUE, Value::Bool(allowed)).unwrap();
            assert_eq!(registry.compile(ROOT).unwrap().validate(&json!(42)).is_ok(), allowed);
            assert_eq!(registry.compile(VALUE).unwrap().validate(&json!(42)).is_ok(), allowed);
        }
    }

    #[test]
    fn duplicate_identity_refuses_without_replacing_the_validator() {
        let mut registry = registry();
        let before = (registry.len(), registry.retained_bytes());
        assert_eq!(registry.insert(VALUE, json!(true)), Err(SchemaRegistryError::DuplicateResource));
        assert_eq!((registry.len(), registry.retained_bytes()), before);
        assert!(registry.compile(ROOT).unwrap().validate(&json!({"value":0})).is_err());
    }

    #[test]
    fn nested_duplicate_identifiers_fail_at_compilation() {
        let mut registry = registry();
        registry.insert("https://schemas.example/other", json!({"$defs":{"duplicate":{
            "$id":VALUE, "type":"string",
        }}})).unwrap();
        let before = (registry.len(), registry.retained_bytes());
        assert!(registry.compile(ROOT).is_err());
        assert_eq!((registry.len(), registry.retained_bytes()), before);
    }

    #[test]
    fn identifier_lookalikes_in_annotations_are_not_resources() {
        let mut registry = SchemaResourceRegistry::default();
        registry.insert(ROOT, json!({"$ref":VALUE,"examples":[{"$id":VALUE,"type":"integer"}]})).unwrap();
        assert!(registry.compile(ROOT).is_err());
        registry.insert(VALUE, json!({"type":"integer"})).unwrap();
        assert!(registry.compile(ROOT).unwrap().validate(&json!(1)).is_ok());
    }

    #[test]
    fn invalid_identity_shape_and_root_alias_leave_accounting_unchanged() {
        let mut registry = registry();
        let before = (registry.len(), registry.retained_bytes());
        for id in ["relative", "https://schemas.example/part#anchor", "https://schemas.example/part#"] {
            assert_eq!(registry.insert(id, json!({})), Err(SchemaRegistryError::InvalidResourceId));
        }
        assert_eq!(registry.insert("https://schemas.example/other", json!({"$id":ROOT})), Err(SchemaRegistryError::IdentityMismatch));
        assert_eq!(registry.insert("https://schemas.example/other", json!([])), Err(SchemaRegistryError::InvalidSchema));
        assert_eq!(registry.insert("https://schemas.example/other", json!({"$defs":[]})), Err(SchemaRegistryError::InvalidSchema));
        assert_eq!((registry.len(), registry.retained_bytes()), before);
    }

    #[test]
    fn resource_capacity_and_encoded_byte_bounds_are_atomic() {
        let limits = SchemaRegistryLimits::new(1, 128, 128).unwrap();
        let mut registry = SchemaResourceRegistry::new(limits);
        assert_eq!(registry.insert(ROOT, json!({"description":"x".repeat(129)})), Err(SchemaRegistryError::ResourceTooLarge));
        assert!(registry.is_empty());
        assert_eq!(registry.retained_bytes(), 0);
        registry.insert(ROOT, json!(true)).unwrap();
        let before = registry.retained_bytes();
        assert_eq!(registry.insert(VALUE, json!(true)), Err(SchemaRegistryError::ResourceLimit));
        assert_eq!(registry.retained_bytes(), before);
        assert!(registry.compile(ROOT).unwrap().validate(&json!(null)).is_ok());
    }

    #[test]
    fn aggregate_retention_counts_resource_keys_and_never_overflows() {
        let schema = json!({"$id":ROOT,"type":"integer"});
        let charged = serde_json::to_vec(&schema).unwrap().len() + ROOT.len();
        let limits = SchemaRegistryLimits::new(2, charged, charged).unwrap();
        let mut registry = SchemaResourceRegistry::new(limits);
        registry.insert(ROOT, schema).unwrap();
        assert_eq!(registry.retained_bytes(), charged);
        assert_eq!(registry.insert(VALUE, json!(true)), Err(SchemaRegistryError::TotalByteLimit));
        assert_eq!(registry.retained_bytes(), charged);
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn deep_and_wide_annotations_are_bounded_before_retention() {
        let mut nested = Value::Null;
        for _ in 0..MAX_SCHEMA_VALIDATION_DEPTH { nested = Value::Array(vec![nested]); }
        let mut registry = SchemaResourceRegistry::default();
        assert_eq!(registry.insert(ROOT, json!({"examples":[nested]})), Err(SchemaRegistryError::NestingLimit));
        let wide = vec![Value::Null; MAX_SCHEMA_ADMISSION_NODES];
        assert_eq!(registry.insert(ROOT, json!({"examples":wide})), Err(SchemaRegistryError::NodeLimit));
        assert!(registry.is_empty());
        assert_eq!(registry.retained_bytes(), 0);
    }

    #[test]
    fn compilation_keeps_shared_vocabulary_admission_in_force() {
        let mut registry = registry();
        registry.insert("https://schemas.example/invalid", json!({"type":"invented"})).unwrap();
        assert!(matches!(registry.compile(ROOT), Err(SchemaRegistryError::Admission(_))));
        assert!(matches!(registry.compile("https://schemas.example/missing"), Err(SchemaRegistryError::UnknownResource)));
    }

    #[test]
    fn compiled_snapshots_are_independent_and_bundles_are_deterministic() {
        let mut first = registry();
        let compiled = first.compile(ROOT).unwrap();
        let mut second = SchemaResourceRegistry::default();
        second.insert(VALUE, json!({"type":"integer","minimum":1})).unwrap();
        second.insert(ROOT, json!({
            "type":"object", "properties":{"value":{"$ref":"value"}},
            "required":["value"], "additionalProperties":false,
        })).unwrap();
        assert_eq!(compiled.schema(), second.compile(ROOT).unwrap().schema());
        first.insert("https://schemas.example/invalid", json!({"type":"invented"})).unwrap();
        assert!(first.compile(ROOT).is_err());
        assert!(compiled.validate(&json!({"value":3})).is_ok());
        assert!(compiled.validate(&json!({"value":0})).is_err());
    }
}
