//! MCP protocol types and JSON-RPC implementation.
//!
//! This crate provides:
//! - JSON-RPC 2.0 message types
//! - MCP-specific method types (tools, resources, prompts)
//! - Protocol version negotiation
//! - Message serialization/deserialization
//!
//! MCP 2026-07-28 support is under implementation and remains unverified.
//! Its core vocabulary is always available. Optional legacy, Tasks, and Apps
//! wire surfaces require their matching crate features and are not aggregate
//! conformance or release evidence.
//!
//! # MCP Protocol Overview
//!
//! MCP (Model Context Protocol) is based on JSON-RPC 2.0 over various transports.
//! The protocol defines:
//!
//! - **Tools**: Executable functions the client can invoke
//! - **Resources**: Data sources the client can read
//! - **Prompts**: Template prompts for the client to use
//!
//! # Wire Format
//!
//! Protocol values serialize as JSON-RPC. Framing is transport-specific; the
//! stdio transport uses newline-delimited JSON (NDJSON).
//!
//! # Role in the System
//!
//! `fastmcp-protocol` is the **shared vocabulary** for FastMCP:
//! - The server uses these types to validate and serialize responses.
//! - The client uses the same types to construct requests and parse replies.
//! - Transports carry these messages without needing to know business logic.
//!
//! If you are integrating FastMCP with a custom runtime or embedding it into
//! another system, depend on this crate to use FastMCP's current JSON-RPC and
//! MCP data models. The modernization disclaimer above still applies.

#![forbid(unsafe_code)]

pub mod common_types;
pub mod extensions;
pub mod http_headers;
#[cfg(feature = "jose")]
pub mod jose;
mod jsonrpc;
#[cfg(feature = "apps")]
pub mod mcp_apps_bridge;
mod messages;
pub mod methods;
pub mod protocol_policy;
pub mod protocol_version;
mod result;
pub mod sampling;
pub mod schema;
pub mod schema_registry;
pub mod security_admission;
pub mod server_discovery;
#[cfg(feature = "tasks")]
pub mod tasks_extension;
mod types;
pub mod uri_template;

pub use common_types::JsonInteger;
pub use extensions::{
    ClientExtensionDiscovery, ExtensionDescriptor, ExtensionDescriptorRegistry, ExtensionDirection,
    ExtensionDiscovery, ExtensionFallbackPolicy, ExtensionHttpEraDisposition, ExtensionId,
    ExtensionMethodDescriptor, ExtensionNegotiationResolver, ExtensionNotificationDescriptor,
    ExtensionRegistryError, ExtensionRegistryReceipt, ExtensionRoutingHeaderDescriptor,
    ExtensionSettings, ExtensionSettingsCompatibilityResolver, ExtensionSettingsResolution,
    ExtensionSettingsSchema, MAX_EXTENSION_DESCRIPTORS, MAX_EXTENSION_ID_BYTES,
    MAX_EXTENSION_REGISTRY_CANONICAL_BYTES, MAX_EXTENSION_SETTINGS_ENTRIES,
    MAX_EXTENSION_SETTINGS_KEY_BYTES, MAX_EXTENSION_SETTINGS_NESTING,
    MAX_EXTENSION_SETTINGS_VALUE_BYTES, MAX_MCP_APPS_MIME_TYPE_BYTES, MAX_MCP_APPS_MIME_TYPES,
    MCP_APPS_ACTIVATION_PREDICATE_ID, MCP_APPS_CLIENT_SETTINGS_SCHEMA_ID,
    MCP_APPS_DOWNLOAD_FILE_METHOD, MCP_APPS_HOST_CONTEXT_CHANGED_NOTIFICATION,
    MCP_APPS_HTML_MIME_TYPE, MCP_APPS_INITIALIZE_METHOD, MCP_APPS_INITIALIZED_NOTIFICATION,
    MCP_APPS_MESSAGE_METHOD, MCP_APPS_NEGOTIATION_RESOLVER_ID, MCP_APPS_OPEN_LINK_METHOD,
    MCP_APPS_REQUEST_DISPLAY_MODE_METHOD, MCP_APPS_REQUEST_TEARDOWN_NOTIFICATION,
    MCP_APPS_RESOURCE_TEARDOWN_METHOD, MCP_APPS_SANDBOX_PROXY_READY_NOTIFICATION,
    MCP_APPS_SANDBOX_RESOURCE_READY_NOTIFICATION, MCP_APPS_SERVER_SETTINGS_SCHEMA_ID,
    MCP_APPS_SIZE_CHANGED_NOTIFICATION, MCP_APPS_TOOL_CANCELLED_NOTIFICATION,
    MCP_APPS_TOOL_INPUT_NOTIFICATION, MCP_APPS_TOOL_INPUT_PARTIAL_NOTIFICATION,
    MCP_APPS_TOOL_RESULT_NOTIFICATION, MCP_APPS_UPDATE_MODEL_CONTEXT_METHOD, McpAppsClientSettings,
    McpAppsNegotiationResolver, OFFICIAL_MCP_APPS_EXTENSION_ID,
    RejectingExtensionNegotiationResolver, ServerExtensionDiscovery, StdioCorrelationDescriptor,
    official_mcp_apps_descriptor, official_mcp_apps_empty_server_settings,
    official_mcp_apps_extension_id, official_mcp_apps_negotiation_resolver,
    register_official_mcp_apps_extension, resolve_official_mcp_apps_settings,
};
#[cfg(feature = "tasks")]
pub use extensions::{
    OFFICIAL_TASKS_EMPTY_SETTINGS_CODEC_ID, OFFICIAL_TASKS_EMPTY_SETTINGS_SCHEMA_ID,
    OFFICIAL_TASKS_EXTENSION_ID, OFFICIAL_TASKS_METHODS, OFFICIAL_TASKS_NOTIFICATION,
    OFFICIAL_TASKS_RESULT_DISCRIMINATOR, OfficialTasksNegotiationResolver,
    TasksNegotiationResolver, official_tasks_descriptor, official_tasks_empty_settings,
    official_tasks_extension_id, register_official_tasks_extension,
};
pub use jsonrpc::{
    ClientIngressFailureScope, CorrelationKey, JSONRPC_VERSION, JsonRpcAdmissionError,
    JsonRpcEndpointRole, JsonRpcError, JsonRpcMessage, JsonRpcMessageDirection, JsonRpcRequest,
    JsonRpcResponse, JsonRpcResponseAdmission, MAX_JSONRPC_STRING_ID_ENCODED_BYTES,
    MAX_RAW_JSON_AGGREGATE_NUMBER_BYTES, MAX_RAW_JSON_CONTAINER_ENTRIES, MAX_RAW_JSON_EXPONENT,
    MAX_RAW_JSON_NESTING_DEPTH, MAX_RAW_JSON_NUMBER_BYTES, MAX_RAW_JSON_PATH_SEGMENT_BYTES,
    MAX_RAW_JSON_PATH_SEGMENTS, RawJsonAdmissionError, RawJsonAdmissionFailure,
    RawJsonRpcDisposition, RawJsonTopLevel, RequestId, UncorrelatedJsonRpcErrorResponse,
    admit_raw_json_document, admit_raw_jsonrpc_document, decode_strict_jsonrpc_message,
    decode_strict_jsonrpc_response, dispose_raw_jsonrpc_failure,
};
#[cfg(feature = "apps")]
pub use mcp_apps_bridge::*;
pub use messages::*;
pub use methods::SERVER_DISCOVER;
pub use protocol_version::{
    FINAL_PROTOCOL_VERSION, FinalHttpRequestMetadata, FinalProtocolVersion, FinalRequestAdmission,
    HEADER_MISMATCH_ERROR_CODE, HEADER_MISMATCH_MESSAGE, HeaderMismatchError, HeaderMismatchReason,
    MAX_REQUIRED_CAPABILITIES_ERROR_DATA_BYTES, MCP_METHOD_HEADER, MCP_NAME_HEADER,
    MCP_PROTOCOL_VERSION_HEADER, MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE,
    MissingRequiredClientCapabilityError, ProtocolVersionError, RequestAdmissionError,
    RequestVersionMetadata, RequiredCapabilitiesError, SUPPORTED_FINAL_PROTOCOL_VERSIONS,
    UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE, UnsupportedProtocolVersionError,
    admit_final_http_request, admit_final_request, validate_final_protocol_version,
};
pub use result::*;
pub use schema::{
    AdmittedFinalFormSchema, AdmittedSchema, FinalCoreResultType, SchemaAdmissionError,
    ValidationError, ValidationResult, admit_final_form_schema, admit_final_schema, validate,
    validate_final_core_result, validate_strict,
};
pub use schema_registry::{SchemaRegistryError, SchemaRegistryLimits, SchemaResourceRegistry};
pub use security_admission::{
    ADMITTED_RSA_PUBLIC_EXPONENT, AdmittedCompactJws, AdmittedRsaPublicJwk, CompactJwsProfile,
    JwkAdmissionPolicy, JwkThumbprintSha256, JwsProtectedHeader, MAX_CLIENT_ID_METADATA_BYTES,
    MAX_CLIENT_REGISTRATION_BYTES, MAX_COMPACT_JWS_ENCODED_BYTES, MAX_JWK_BYTES, MAX_JWK_KID_BYTES,
    MAX_JWK_SET_BYTES, MAX_JWK_SET_KEYS, MAX_JWS_CLAIMS_BYTES, MAX_JWS_PROTECTED_HEADER_BYTES,
    MAX_JWS_SIGNATURE_BYTES, MAX_OAUTH_METADATA_BYTES, MAX_OIDC_PROVIDER_METADATA_BYTES,
    MAX_PROTECTED_RESOURCE_METADATA_BYTES, MAX_RFC7638_CANONICAL_INPUT_BYTES,
    MAX_RSA_MODULUS_BYTES, MAX_TOKEN_RESPONSE_BYTES, MIN_RSA_MODULUS_BYTES, SecurityAdmissionError,
    SecurityDocumentKind, admit_compact_jws, admit_public_jwk_set, admit_public_rsa_components,
    admit_public_rsa_jwk, admit_security_document, admit_security_document_object,
};
pub use server_discovery::{
    DiscoveryCacheHints, MAX_SERVER_INSTRUCTIONS_BYTES, SERVER_DISCOVER_METHOD,
    SERVER_DISCOVER_SERVER_INFO_META_KEY, SERVER_DISCOVER_SUPPORTED_VERSIONS, ServerBehavior,
    ServerBehaviorRegistry, ServerDiscoverCapabilities, ServerDiscoverRequest,
    ServerDiscoverResult, ServerDiscoveryError, ServerInstructionError, ServerInstructions,
};
#[cfg(feature = "tasks")]
pub use tasks_extension::{
    CancelTaskParams as FinalCancelTaskParams, CancelTaskResult as FinalCancelTaskResult,
    CompleteTaskResult, CreateTaskResult, EmptyTaskResult, FinalTaskCallToolResult, FinalTaskError,
    GetTaskParams as FinalGetTaskParams, GetTaskResult as FinalGetTaskResult, MAX_TASK_ID_BYTES,
    MAX_TASK_INPUT_MAP_ENTRIES, MAX_TASK_SUBSCRIPTION_IDS, RELATED_TASK_META_KEY, TASK_CANCEL,
    TASK_GET, TASK_STATUS_NOTIFICATION, TASK_SUBSCRIPTION_IDS_KEY, TASKS_EXTENSION, Task, TaskBase,
    TaskDuration, TaskId as FinalTaskId, TaskInputLedger, TaskInputRequests, TaskInputResponses,
    TaskMethodRequest, TaskRequestMeta, TaskStatus as FinalTaskStatus, TaskStatusNotification,
    TaskStatusNotificationParams as FinalTaskStatusNotificationParams, TaskTimestamp,
    TaskWireError, UpdateTaskParams, UpdateTaskResult, set_task_subscription_ids,
    task_subscription_ids,
};
pub use types::*;
pub use uri_template::{
    MAX_URI_TEMPLATE_BYTES, MAX_URI_TEMPLATE_COMPOSITE_ITEMS,
    MAX_URI_TEMPLATE_EXPANSION_OUTPUT_BYTES, MAX_URI_TEMPLATE_EXPRESSIONS, MAX_URI_TEMPLATE_PARTS,
    MAX_URI_TEMPLATE_PREFIX_LENGTH, MAX_URI_TEMPLATE_VALUE_BYTES,
    MAX_URI_TEMPLATE_VARIABLE_NAME_BYTES, MAX_URI_TEMPLATE_VARIABLES_PER_EXPRESSION,
    ReversibleResourceTemplate, TemplateValue, TemplateValues, UriTemplate, UriTemplateError,
    UriTemplateExpansionLimits, UriTemplateExpression, UriTemplateModifier, UriTemplateOperator,
    UriTemplatePart,
};

// The FND-03 contract freezes unqualified `cargo test -- --exact` IDs. Keep
// the executable entry points at the crate root while retaining their full
// assertions beside the policy implementation.
#[cfg(test)]
#[test]
fn fnd_03_policy_receipts_positive() {
    protocol_policy::tests::fnd_03_policy_receipts_positive();
}

#[cfg(test)]
#[test]
fn fnd_03_policy_receipts_planted_negative() {
    protocol_policy::tests::fnd_03_policy_receipts_planted_negative();
}

// These two deliberately do NOT carry the frozen FND-03 A acceptance IDs, for
// the same two reasons as the B pair below. A `cfg(test)` function cannot
// prove shipped behavior (PL-3), and a frozen ID must resolve to exactly one
// definition workspace-wide (PL-1) so that `--exact <bare name>` discovers
// exactly one test. The frozen `fnd_03_a_positive` /
// `fnd_03_a_planted_negative` pair lives in the external public-surface
// consumer at `crates/fastmcp-protocol/tests/fnd_03_a.rs`. These retain their
// assertions as in-crate coverage.
#[cfg(test)]
#[test]
fn fnd_03_a_unit_positive() {
    protocol_policy::tests::fnd_03_policy_receipts_positive();
}

#[cfg(test)]
#[test]
fn fnd_03_a_unit_planted_negative() {
    protocol_policy::tests::fnd_03_policy_receipts_planted_negative();
}

#[cfg(test)]
#[test]
fn fnd_03_era_classification_positive() {
    protocol_policy::tests::fnd_03_era_classification_positive();
}

#[cfg(test)]
#[test]
fn fnd_03_era_classification_planted_negative() {
    protocol_policy::tests::fnd_03_era_classification_planted_negative();
}

// These two deliberately do NOT carry the frozen FND-03 B acceptance IDs. A
// `cfg(test)` function cannot prove shipped behavior (PL-3), and a frozen ID
// must resolve to exactly one definition workspace-wide (PL-1) so that
// `cargo test ... <id> -- --exact` discovers exactly one test. The frozen
// `fnd_03_b_positive` / `fnd_03_b_planted_negative` pair lives in the external
// public-surface consumer at `crates/fastmcp-protocol/tests/fnd_03_b.rs`.
#[cfg(test)]
#[test]
fn fnd_03_b_unit_positive() {
    protocol_policy::tests::fnd_03_era_classification_positive();
}

#[cfg(test)]
#[test]
fn fnd_03_b_unit_planted_negative() {
    protocol_policy::tests::fnd_03_era_classification_planted_negative();
}

// The frozen `prt_03_i_positive` / `prt_03_i_planted_negative` pair lives in the
// external public-surface consumer at
// `crates/fastmcp-protocol/tests/prt_03_integration.rs`, matching the
// `fnd_03_b_*` arrangement above. These two keep the same coverage as an
// in-crate unit rehearsal under distinct names.
//
// They were renamed rather than removed because their content is substantive,
// not vestigial. What made the original names wrong was placement: at the crate
// root they took the BARE libtest names `prt_03_i_positive` /
// `prt_03_i_planted_negative`, so `--exact <bare name>` would have matched both
// them and the external pair — two discovered tests for one required ID. They
// also could not satisfy the bead's own predicate, which requires the behaviour
// to be present "through the shipped, non-`cfg(test)` public surface". Nothing
// here needs crate-internal access: every symbol below is publicly re-exported,
// so the external consumer runs this same content as a genuine downstream user.
#[cfg(test)]
#[test]
fn prt_03_i_unit_positive() {
    let required_capabilities = ClientCapabilities {
        roots: Some(RootsCapability { list_changed: true }),
        ..ClientCapabilities::default()
    };
    let metadata = FinalRequestMeta::new(required_capabilities.clone());
    let admission = admit_final_http_request(FinalHttpRequestMetadata {
        version: metadata.version_metadata(Some(FINAL_PROTOCOL_VERSION)),
        header_method: Some(SERVER_DISCOVER),
        body_method: Some(SERVER_DISCOVER),
        header_name: None,
        body_name: None,
    })
    .expect("canonical final metadata and server discovery must be admitted");
    let missing =
        MissingRequiredClientCapabilityError::from_client_capabilities(&required_capabilities)
            .expect("typed required capabilities must encode as final error data");

    assert_eq!(FINAL_PROTOCOL_VERSION, "2026-07-28");
    assert_eq!(SERVER_DISCOVER, SERVER_DISCOVER_METHOD);
    assert_eq!(MCP_PROTOCOL_VERSION_HEADER, "MCP-Protocol-Version");
    assert_eq!(MCP_METHOD_HEADER, "Mcp-Method");
    assert_eq!(MCP_NAME_HEADER, "Mcp-Name");
    assert_eq!(
        admission.protocol_version().as_str(),
        FINAL_PROTOCOL_VERSION
    );
    assert_eq!(missing.http_status(), 400);
    assert_eq!(
        missing.jsonrpc_error_code(),
        MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE
    );
    assert_eq!(
        missing.canonical_error_data(),
        serde_json::json!({"requiredCapabilities": {"roots": {"listChanged": true}}})
    );
}

#[cfg(test)]
#[test]
fn prt_03_i_unit_planted_negative() {
    let metadata = FinalRequestMeta::new(ClientCapabilities {
        roots: Some(RootsCapability { list_changed: true }),
        ..ClientCapabilities::default()
    });
    let wire_before = serde_json::to_value(&metadata).expect("metadata serializes");
    let error = admit_final_http_request(FinalHttpRequestMetadata {
        version: metadata.version_metadata(Some("2025-11-25")),
        header_method: Some(SERVER_DISCOVER),
        body_method: Some(SERVER_DISCOVER),
        header_name: None,
        body_name: None,
    })
    .expect_err("changing only the protocol header must reject the request");

    assert!(
        matches!(&error, RequestAdmissionError::HeaderMismatch(_)),
        "a mismatched version mirror must precede unsupported-version classification"
    );
    let RequestAdmissionError::HeaderMismatch(error) = error else {
        return;
    };
    assert_eq!(
        error.reason(),
        HeaderMismatchReason::HeaderBodyVersionMismatch
    );
    assert_eq!(error.http_status(), 400);
    assert_eq!(error.jsonrpc_error_code(), HEADER_MISMATCH_ERROR_CODE);
    assert_eq!(error.canonical_error_data(), None);
    assert_eq!(
        serde_json::to_value(&metadata).expect("metadata remains serializable"),
        wire_before
    );
}
