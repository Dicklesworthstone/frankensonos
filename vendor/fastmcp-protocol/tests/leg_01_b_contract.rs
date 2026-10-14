//! Frozen top-level LEG-01 B translation and disposition acceptance entries.

use fastmcp_protocol::methods::{
    Legacy2024ResultDisposition, Legacy2024ResultKind, PING, PROMPTS_GET, RESOURCES_READ,
    TOOLS_CALL, classify_legacy_2024_result, translate_legacy_2024_result,
};
use serde_json::{Value, json};

fn exact_tool_result() -> Value {
    json!({
        "content": [{
            "type": "text",
            "text": "legacy tool output",
            "annotations": {"audience": ["user"], "priority": 0.5}
        }],
        "isError": false,
        "_meta": {"trace": "legacy"},
    })
}

fn exact_resource_result() -> Value {
    json!({
        "contents": [{
            "uri": "file:///legacy.txt",
            "mimeType": "text/plain",
            "text": "legacy resource"
        }],
        "_meta": {"trace": "legacy"},
    })
}

fn exact_prompt_result() -> Value {
    json!({
        "description": "legacy prompt",
        "messages": [{
            "role": "assistant",
            "content": {
                "type": "resource",
                "resource": {"uri": "file:///prompt.txt", "blob": "bGVnYWN5"}
            }
        }],
    })
}

#[test]
fn leg_01_translation_positive() {
    for (method, result, expected_kind) in [
        (TOOLS_CALL, exact_tool_result(), Legacy2024ResultKind::Tool),
        (
            RESOURCES_READ,
            exact_resource_result(),
            Legacy2024ResultKind::Resource,
        ),
        (
            PROMPTS_GET,
            exact_prompt_result(),
            Legacy2024ResultKind::Prompt,
        ),
    ] {
        assert_eq!(
            classify_legacy_2024_result(method, &result)
                .expect("ordinary complete result must have an exact disposition"),
            Legacy2024ResultDisposition::Lossless(expected_kind)
        );
        assert_eq!(
            translate_legacy_2024_result(method, result.clone())
                .expect("ordinary complete result must remain field-for-field lossless"),
            result
        );
    }
}

#[test]
fn leg_01_translation_planted_negative() {
    let original = exact_tool_result();
    let mut malformed = original.clone();
    malformed["content"][0]["type"] = json!("audio");

    assert_eq!(
        translate_legacy_2024_result(TOOLS_CALL, malformed)
            .expect_err(
                "changing only the content discriminator must reject an unrepresentable item"
            )
            .reason(),
        "exact MCP 2024-11-05 content must be text, image, or resource"
    );
    assert_eq!(original["content"][0]["type"], "text");
}

#[test]
fn leg_01_disposition_positive() {
    assert_eq!(
        classify_legacy_2024_result(PING, &json!({}))
            .expect("an exact legacy-owned method must receive the legacy-owned disposition"),
        Legacy2024ResultDisposition::LegacyOwned
    );
    assert_eq!(
        translate_legacy_2024_result(PING, json!({}))
            .expect_err("legacy-owned results must not fall through to shared translation")
            .reason(),
        "method result is owned by the exact MCP 2024-11-05 adapter"
    );
}

#[test]
fn leg_01_disposition_planted_negative() {
    let original = exact_resource_result();
    let mut modern_metadata = original.clone();
    modern_metadata["structuredContent"] = json!({"answer": 42});

    assert_eq!(
        classify_legacy_2024_result(RESOURCES_READ, &modern_metadata)
            .expect_err("changing only to modern metadata must reject before translation")
            .reason(),
        "modern-only result member cannot be represented by exact MCP 2024-11-05"
    );
    assert_eq!(original.get("structuredContent"), None);
}

#[test]
fn legacy_unclassified_result_member_planted_negative() {
    let original = exact_tool_result();
    let mut unclassified = original.clone();
    unclassified["legacyVendor"] = json!({"opaque": true});

    assert_eq!(
        classify_legacy_2024_result(TOOLS_CALL, &unclassified)
            .expect_err("changing only to an unclassified result member must reject it")
            .reason(),
        "unclassified result member cannot be represented by exact MCP 2024-11-05"
    );
    assert_eq!(original.get("legacyVendor"), None);
}

#[test]
fn leg_01_b_positive() {
    let result = exact_prompt_result();

    assert_eq!(
        classify_legacy_2024_result(PROMPTS_GET, &result)
            .expect("exact prompt output must have a lossless disposition"),
        Legacy2024ResultDisposition::Lossless(Legacy2024ResultKind::Prompt)
    );
    assert_eq!(
        translate_legacy_2024_result(PROMPTS_GET, result.clone())
            .expect("exact prompt output must translate without alteration"),
        result
    );
}

#[test]
fn leg_01_b_planted_negative() {
    let original = exact_resource_result();
    let mut malformed = original.clone();
    malformed["contents"][0]["text"] = json!(false);

    assert_eq!(
        translate_legacy_2024_result(RESOURCES_READ, malformed)
            .expect_err("changing only a resource data field must reject the malformed output")
            .reason(),
        "exact MCP 2024-11-05 resource contents require string uri and text or blob data"
    );
    assert_eq!(original["contents"][0]["text"], "legacy resource");
}
