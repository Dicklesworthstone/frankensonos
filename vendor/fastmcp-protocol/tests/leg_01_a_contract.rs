//! Literal frozen LEG-01 A runner entries.

use std::fmt::Write as _;

use fastmcp_core::sha256_bounded;
use fastmcp_protocol::methods::{
    INITIALIZE, LEGACY_2024_11_05_METHODS, LEGACY_2024_11_05_PROTOCOL_VERSION,
    LEGACY_2024_11_05_SCHEMA_SHA256, Legacy2024Capability, Legacy2024Direction, Legacy2024Envelope,
    Legacy2024EnvelopeKind, NOTIFICATIONS_INITIALIZED, SAMPLING_CREATE_MESSAGE, TOOLS_CALL,
    decode_legacy_2024_11_05_envelope, legacy_2024_11_05_method, legacy_2024_11_05_schema,
    translate_legacy_2024_result,
};
use serde_json::{Value, json};

fn exact_initialize() -> Value {
    json!({
        "jsonrpc": "2.0", "id": "legacy-a", "method": "initialize",
        "params": {
            "protocolVersion": LEGACY_2024_11_05_PROTOCOL_VERSION,
            "capabilities": {"sampling": {}, "roots": {"listChanged": true}},
            "clientInfo": {"name": "legacy-public-consumer", "version": "1.0.0"},
        },
    })
}

fn exact_sampling_request() -> Value {
    json!({
        "jsonrpc": "2.0", "id": "legacy-sampling", "method": SAMPLING_CREATE_MESSAGE,
        "params": {
            "maxTokens": 128,
            "includeContext": "thisServer",
            "messages": [{
                "role": "user",
                "content": {
                    "type": "text", "text": "Summarize this",
                    "annotations": {"audience": ["assistant"], "priority": 1.0}
                }
            }],
            "modelPreferences": {"hints": [{"name": "exact-legacy"}], "costPriority": 0.5},
            "stopSequences": ["END"],
            "metadata": {"trace": "legacy"},
        },
    })
}

#[test]
fn leg_01_schema_parity_positive() {
    let schema = legacy_2024_11_05_schema().expect("pinned exact schema must parse");

    assert_eq!(schema["$schema"], "http://json-schema.org/draft-07/schema#");
    assert_eq!(
        schema["definitions"]["InitializeRequest"]["properties"]["method"]["const"],
        INITIALIZE
    );
    assert_eq!(
        schema["definitions"]["ClientCapabilities"]["properties"].get("elicitation"),
        None
    );
    assert_eq!(
        LEGACY_2024_11_05_SCHEMA_SHA256,
        "61cea2392d4f284092d09bc84b9ac488c0d5618ac2b38a56942fc5b99fd960ce"
    );
}

#[test]
fn leg_01_schema_parity_planted_negative() {
    let mut wrong_era = exact_initialize();
    wrong_era["params"]["protocolVersion"] = json!(false);

    assert_eq!(
        decode_legacy_2024_11_05_envelope(wrong_era)
            .expect_err("changing only the protocol version from a string must reject")
            .reason(),
        "initialize protocolVersion must be a string"
    );
}

#[test]
fn leg_01_method_inventory_positive() {
    let expected = [
        (
            "initialize",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            None,
        ),
        (
            "notifications/initialized",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Notification,
            None,
        ),
        (
            "ping",
            Legacy2024Direction::Bidirectional,
            Legacy2024EnvelopeKind::Request,
            None,
        ),
        (
            "tools/list",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerTools),
        ),
        (
            "tools/call",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerTools),
        ),
        (
            "resources/list",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerResources),
        ),
        (
            "resources/templates/list",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerResources),
        ),
        (
            "resources/read",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerResources),
        ),
        (
            "resources/subscribe",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerResourcesSubscribe),
        ),
        (
            "resources/unsubscribe",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerResourcesSubscribe),
        ),
        (
            "prompts/list",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerPrompts),
        ),
        (
            "prompts/get",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerPrompts),
        ),
        (
            "logging/setLevel",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ServerLogging),
        ),
        (
            "completion/complete",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Request,
            None,
        ),
        (
            "sampling/createMessage",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ClientSampling),
        ),
        (
            "roots/list",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Request,
            Some(Legacy2024Capability::ClientRoots),
        ),
        (
            "notifications/cancelled",
            Legacy2024Direction::Bidirectional,
            Legacy2024EnvelopeKind::Notification,
            None,
        ),
        (
            "notifications/progress",
            Legacy2024Direction::Bidirectional,
            Legacy2024EnvelopeKind::Notification,
            None,
        ),
        (
            "notifications/roots/list_changed",
            Legacy2024Direction::ClientToServer,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ClientRootsListChanged),
        ),
        (
            "notifications/message",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ServerLogging),
        ),
        (
            "notifications/prompts/list_changed",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ServerPromptsListChanged),
        ),
        (
            "notifications/resources/list_changed",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ServerResourcesListChanged),
        ),
        (
            "notifications/resources/updated",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ServerResourcesSubscribe),
        ),
        (
            "notifications/tools/list_changed",
            Legacy2024Direction::ServerToClient,
            Legacy2024EnvelopeKind::Notification,
            Some(Legacy2024Capability::ServerToolsListChanged),
        ),
    ];
    let actual: Vec<_> = LEGACY_2024_11_05_METHODS
        .iter()
        .map(|method| {
            (
                method.name,
                method.direction,
                method.envelope,
                method.capability,
            )
        })
        .collect();

    assert_eq!(actual, expected);
    assert_eq!(legacy_2024_11_05_method("elicitation/create"), None);
    let sampling = legacy_2024_11_05_method(SAMPLING_CREATE_MESSAGE)
        .expect("the exact legacy sampling method must be present");
    assert_eq!(sampling.direction, Legacy2024Direction::ServerToClient);
    assert_eq!(sampling.envelope, Legacy2024EnvelopeKind::Request);
    assert_eq!(
        sampling.capability,
        Some(Legacy2024Capability::ClientSampling)
    );
}

#[test]
fn leg_01_method_inventory_planted_negative() {
    let mut modern_method = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/list"});
    modern_method["method"] = json!("elicitation/create");

    assert_eq!(
        decode_legacy_2024_11_05_envelope(modern_method)
            .expect_err("changing only the method to elicitation must reject it")
            .reason(),
        "method is not part of exact MCP 2024-11-05"
    );
}

#[test]
fn leg_01_envelopes_positive() {
    assert!(matches!(
        decode_legacy_2024_11_05_envelope(exact_initialize())
            .expect("an exact initialize request must decode"),
        Legacy2024Envelope::Request { method, id, .. }
            if method.name == INITIALIZE && id == json!("legacy-a")
    ));
    assert!(matches!(
        decode_legacy_2024_11_05_envelope(json!({"jsonrpc": "2.0", "id": 1, "result": {}}))
            .expect("an object result envelope must decode"),
        Legacy2024Envelope::Response { id, result } if id == json!(1) && result == json!({})
    ));
    assert!(matches!(
        decode_legacy_2024_11_05_envelope(json!({
            "jsonrpc": "2.0", "method": NOTIFICATIONS_INITIALIZED
        }))
        .expect("an exact notification must decode"),
        Legacy2024Envelope::Notification { method, .. } if method.name == NOTIFICATIONS_INITIALIZED
    ));
}

#[test]
fn leg_01_legacy_response_metadata_is_separate_from_final_metadata() {
    let legacy_response = json!({
        "jsonrpc": "2.0",
        "id": "legacy-reverse",
        "result": {
            "roots": [],
            "_meta": {"com.example/opaque": {"retained": true}},
        },
    });

    assert!(matches!(
        decode_legacy_2024_11_05_envelope(legacy_response.clone())
            .expect("application-defined legacy response metadata must remain representable"),
        Legacy2024Envelope::Response { id, result }
            if id == json!("legacy-reverse") && result == legacy_response["result"]
    ));

    let mut final_metadata = legacy_response.clone();
    final_metadata["result"]["_meta"]["io.modelcontextprotocol/protocolVersion"] =
        json!("2026-07-28");
    assert_eq!(
        decode_legacy_2024_11_05_envelope(final_metadata)
            .expect_err("changing only to final metadata must reject the legacy response")
            .reason(),
        "final result members cannot be represented by exact MCP 2024-11-05"
    );
    assert_eq!(
        legacy_response["result"]["_meta"],
        json!({"com.example/opaque": {"retained": true}}),
        "cross-era rejection must not alter the legacy response baseline"
    );
}

#[test]
fn leg_01_top_level_batch_array_planted_negative() {
    let array_of_one = Value::Array(vec![exact_initialize()]);
    let mixed_array = Value::Array(vec![
        exact_initialize(),
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    ]);
    let expected =
        "MCP 2024-11-05 requires one top-level JSON-RPC object; batch arrays are unsupported";

    assert_eq!(
        decode_legacy_2024_11_05_envelope(array_of_one)
            .expect_err(
                "an array containing one otherwise-valid request must reject before dispatch"
            )
            .reason(),
        expected
    );
    assert_eq!(
        decode_legacy_2024_11_05_envelope(mixed_array)
            .expect_err("a mixed batch array must reject before dispatch")
            .reason(),
        expected
    );
}

#[test]
fn leg_01_open_capability_extension_positive() {
    let mut modern_capability = exact_initialize();
    modern_capability["params"]["capabilities"]["extensions"] = json!({"io.example/modern": {}});

    let Legacy2024Envelope::Request {
        params: Some(params),
        ..
    } = decode_legacy_2024_11_05_envelope(modern_capability)
        .expect("the exact 2024 capability object is schema-open")
    else {
        panic!("initialize must remain a request with params");
    };
    assert_eq!(
        params["capabilities"]["extensions"],
        json!({"io.example/modern": {}}),
        "unknown capability members are retained inert for later era policy"
    );
}

#[test]
fn legacy_sampling_message_planted_negative() {
    let original = exact_sampling_request();
    let mut invalid_message = original.clone();
    invalid_message["params"]["messages"][0]["role"] = json!("system");

    assert_eq!(
        decode_legacy_2024_11_05_envelope(invalid_message)
            .expect_err("changing only a sampling role must reject the legacy message")
            .reason(),
        "sampling/createMessage messages require an exact user or assistant role"
    );
    assert_eq!(original["params"]["messages"][0]["role"], "user");
}

#[test]
fn leg_01_a_positive() {
    let schema = legacy_2024_11_05_schema().expect("pinned exact schema must parse");
    assert_eq!(LEGACY_2024_11_05_METHODS.len(), 24);
    assert_eq!(
        schema["definitions"]["InitializeRequest"]["properties"]["method"]["const"],
        "initialize"
    );
    assert_eq!(
        schema["definitions"]["ClientCapabilities"]["properties"].get("elicitation"),
        None
    );
    assert_eq!(
        LEGACY_2024_11_05_SCHEMA_SHA256,
        "61cea2392d4f284092d09bc84b9ac488c0d5618ac2b38a56942fc5b99fd960ce"
    );
    let digest = sha256_bounded(
        fastmcp_protocol::methods::LEGACY_2024_11_05_SCHEMA_JSON.as_bytes(),
        fastmcp_protocol::methods::LEGACY_2024_11_05_SCHEMA_JSON.len(),
    )
    .expect("pinned schema has a fixed bounded size");
    let mut checksum = String::with_capacity(digest.as_bytes().len() * 2);
    for byte in digest.as_bytes() {
        write!(checksum, "{byte:02x}").expect("writing hexadecimal bytes to a String cannot fail");
    }
    assert_eq!(checksum, LEGACY_2024_11_05_SCHEMA_SHA256);
    assert!(matches!(
        decode_legacy_2024_11_05_envelope(exact_initialize())
            .expect("public exact initialize must decode"),
        Legacy2024Envelope::Request { method, id, .. }
            if method.name == "initialize" && id == json!("legacy-a")
    ));
    assert!(matches!(
        decode_legacy_2024_11_05_envelope(exact_sampling_request())
            .expect("an exact sampling request must validate every nested legacy field"),
        Legacy2024Envelope::Request { method, id, .. }
            if method.name == SAMPLING_CREATE_MESSAGE && id == json!("legacy-sampling")
    ));
    let ordinary = json!({"content": [{"type": "text", "text": "legacy"}]});
    assert_eq!(
        translate_legacy_2024_result(TOOLS_CALL, ordinary.clone())
            .expect("ordinary complete tool result must remain lossless"),
        ordinary
    );
}

#[test]
fn leg_01_a_planted_negative() {
    let original = exact_sampling_request();
    let mut invalid_context = original.clone();
    invalid_context["params"]["includeContext"] = json!("anotherServer");

    assert_eq!(
        decode_legacy_2024_11_05_envelope(invalid_context)
            .expect_err("changing only includeContext must reject the legacy sampling request")
            .reason(),
        "sampling/createMessage includeContext must be allServers, none, or thisServer"
    );
    assert_eq!(original["params"]["includeContext"], "thisServer");
}
