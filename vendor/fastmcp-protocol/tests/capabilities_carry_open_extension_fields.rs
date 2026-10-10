//! bd-jsm7z / bd-0jmhb: the two schema-declared open fields must survive the wire.
//!
//! `ClientCapabilities` and `ServerCapabilities` each dropped `experimental`
//! and `extensions` on every deserialization, because neither field was
//! declared on the Rust type and neither type carries `deny_unknown_fields` or
//! a flatten-capture. The fields landed at commit:9ba90837 and commit:6c0ada34
//! respectively; this target is the proof that they are carried, and nothing
//! else in the workspace exercises them.
//!
//! THE WORTHLESS VERSION OF THIS FILE, NAMED FIRST. Build a value with
//! `extensions: Some(map)`, serialize it, deserialize it, assert equality.
//! That passes whether or not `deny_unknown_fields` is present, whether or not
//! a non-object extension value stays representable, and whether or not the
//! field is omitted when unset. It also cannot fail on the original defect at
//! all: the struct literal simply would not compile against the pre-fix type,
//! so it detects nothing at run time.
//!
//! Every assertion below therefore starts from WIRE JSON and reads the
//! RE-SERIALIZED form by string key. That shape compiles unchanged against the
//! pre-fix type and fails on it, which is what makes it a regression test
//! rather than a restatement of the struct definition.
//!
//! These tests live in `tests/`, a separate compilation unit that links the
//! crate as an ordinary dependent and cannot see `#[cfg(test)]` items, so they
//! prove the shipped public surface.

use fastmcp_protocol::{ClientCapabilities, ServerCapabilities};
use serde_json::{Value, json};

/// The identifier the schema itself uses as its worked `extensions` example,
/// and the constant the blocked bd-460kj tests plant.
const CLIENT_CREDENTIALS_EXTENSION: &str = "io.modelcontextprotocol/oauth-client-credentials";

/// A key that is not declared on either capability type.
const UNDECLARED_MEMBER: &str = "notADeclaredCapabilityMember";

/// Deserialize `wire` as `T` and return its re-serialized JSON form.
///
/// Reading the result by string key is deliberate: it is the only observation
/// that the pre-fix type could also have been subjected to.
fn round_trip<T>(wire: &Value) -> Value
where
    T: serde::de::DeserializeOwned + serde::Serialize,
{
    let typed: T = serde_json::from_value(wire.clone())
        .expect("capabilities deserialize from a schema-conformant wire object");
    serde_json::to_value(&typed).expect("capabilities serialize back to JSON")
}

/// A wire object populating both open fields alongside one ordinary capability.
fn populated_client_wire() -> Value {
    json!({
        "roots": {"listChanged": true},
        "experimental": {"vendor.streaming": {"maxFrames": 8}},
        "extensions": {CLIENT_CREDENTIALS_EXTENSION: {}},
    })
}

/// The server-side mirror of [`populated_client_wire`].
fn populated_server_wire() -> Value {
    json!({
        "tools": {"listChanged": true},
        "experimental": {"vendor.batching": {"maxBatch": 16}},
        "extensions": {CLIENT_CREDENTIALS_EXTENSION: {}},
    })
}

/// `ClientCapabilities` carries both open fields from the wire and emits them
/// again unchanged.
///
/// This is the assertion the defect broke: before commit:9ba90837 both lookups
/// returned `None` because serde had no field to bind them to.
#[test]
fn client_capabilities_carry_both_open_fields_across_a_wire_round_trip() {
    let wire = populated_client_wire();
    let emitted = round_trip::<ClientCapabilities>(&wire);

    assert_eq!(
        emitted.get("experimental"),
        wire.get("experimental"),
        "ClientCapabilities.experimental was not carried across the round trip; a schema-declared \
         field that deserializes to nothing is the bd-jsm7z defect"
    );
    assert_eq!(
        emitted.get("extensions"),
        wire.get("extensions"),
        "ClientCapabilities.extensions was not carried across the round trip; this is the \
         precondition the client_credentials negotiation guard reads as caps.get(\"extensions\")"
    );
}

/// `ServerCapabilities` carries both open fields from the wire and emits them
/// again unchanged.
#[test]
fn server_capabilities_carry_both_open_fields_across_a_wire_round_trip() {
    let wire = populated_server_wire();
    let emitted = round_trip::<ServerCapabilities>(&wire);

    assert_eq!(
        emitted.get("experimental"),
        wire.get("experimental"),
        "ServerCapabilities.experimental was not carried across the round trip; a schema-declared \
         field that deserializes to nothing is the bd-0jmhb defect"
    );
    assert_eq!(
        emitted.get("extensions"),
        wire.get("extensions"),
        "ServerCapabilities.extensions was not carried across the round trip"
    );
}

/// An undeclared sibling member must not fail the whole deserialization, and
/// must be DROPPED rather than preserved.
///
/// `additionalProperties` is UNSET on `ClientCapabilities` in the 2026-07-28
/// schema, i.e. open, so rejecting an unknown member would be non-conformant.
/// This is a FORWARD guard, not a proof of bd-jsm7z: it passed before the fix
/// too, because the fix did not change unknown-member handling. What it fires
/// on is a later `deny_unknown_fields`, or a later flatten-capture field.
///
/// If the drop assertion below ever needs to change, that is a deliberate wire
/// decision and not a test repair: preserving unknown members would mean the
/// type round-trips arbitrary JSON, which removes the reason `extensions` had
/// to be declared at all.
#[test]
fn client_capabilities_ignore_an_undeclared_sibling_member_without_rejecting_it() {
    let mut wire = populated_client_wire();
    wire[UNDECLARED_MEMBER] = json!({"anything": 1});

    let typed: Result<ClientCapabilities, _> = serde_json::from_value(wire);
    let typed = typed.expect(
        "an undeclared sibling member must not fail ClientCapabilities deserialization; \
         additionalProperties is UNSET on this type in the schema, so it is open",
    );

    let emitted = serde_json::to_value(&typed).expect("capabilities serialize back to JSON");
    assert!(
        emitted.get(UNDECLARED_MEMBER).is_none(),
        "the undeclared member was preserved rather than dropped, so the type now round-trips \
         arbitrary JSON; see this test's doc comment before changing it"
    );
    assert_eq!(
        emitted.get("extensions"),
        Some(&json!({CLIENT_CREDENTIALS_EXTENSION: {}})),
        "the declared open field must still be carried when an undeclared member is present"
    );
}

/// The server-side mirror of the undeclared-member rule.
#[test]
fn server_capabilities_ignore_an_undeclared_sibling_member_without_rejecting_it() {
    let mut wire = populated_server_wire();
    wire[UNDECLARED_MEMBER] = json!({"anything": 1});

    let typed: Result<ServerCapabilities, _> = serde_json::from_value(wire);
    let typed = typed.expect(
        "an undeclared sibling member must not fail ServerCapabilities deserialization; \
         additionalProperties is UNSET on this type in the schema, so it is open",
    );

    let emitted = serde_json::to_value(&typed).expect("capabilities serialize back to JSON");
    assert!(
        emitted.get(UNDECLARED_MEMBER).is_none(),
        "the undeclared member was preserved rather than dropped, so the type now round-trips \
         arbitrary JSON; see this test's doc comment before changing it"
    );
}

/// A non-object extension VALUE must stay representable rather than failing the
/// whole deserialization.
///
/// This is the property the `serde_json::Value` element type was chosen for. A
/// nested `BTreeMap<String, BTreeMap<String, Value>>` would reject the document
/// outright, which loses every other capability in it and leaves the consumer
/// nothing to refuse. The schema's rule -- an extension value is a settings
/// object -- is then enforced at the consumer's typed boundary, which reads
/// exactly the `as_object()` that this test asserts is `None`.
#[test]
fn client_capabilities_keep_a_non_object_extension_value_representable() {
    let wire = json!({
        "extensions": {CLIENT_CREDENTIALS_EXTENSION: "not-a-settings-object"},
    });
    let emitted = round_trip::<ClientCapabilities>(&wire);

    let value = emitted
        .get("extensions")
        .and_then(|extensions| extensions.get(CLIENT_CREDENTIALS_EXTENSION))
        .expect("a non-object extension value is carried rather than discarded");
    assert_eq!(
        value,
        &json!("not-a-settings-object"),
        "the non-object value must survive verbatim so a consumer can refuse it"
    );
    assert!(
        value.as_object().is_none(),
        "as_object() must report the value is not a settings object; this is the exact check the \
         client_credentials negotiation guard performs before returning Negotiation"
    );
}

/// The server-side mirror of the non-object-value rule.
#[test]
fn server_capabilities_keep_a_non_object_extension_value_representable() {
    let wire = json!({
        "extensions": {CLIENT_CREDENTIALS_EXTENSION: 7},
    });
    let emitted = round_trip::<ServerCapabilities>(&wire);

    let value = emitted
        .get("extensions")
        .and_then(|extensions| extensions.get(CLIENT_CREDENTIALS_EXTENSION))
        .expect("a non-object extension value is carried rather than discarded");
    assert_eq!(
        value,
        &json!(7),
        "the non-object value must survive verbatim so a consumer can refuse it"
    );
    assert!(
        value.as_object().is_none(),
        "as_object() must report the value is not a settings object"
    );
}

/// Neither open field appears on the wire when it is unset.
///
/// `skip_serializing_if = "Option::is_none"` is what made adding these fields a
/// purely additive change: every existing server and client emits exactly the
/// bytes it emitted before. Dropping the attribute would start sending
/// `"experimental": null` to every peer, which is a wire change disguised as a
/// refactor.
#[test]
fn capabilities_omit_both_open_fields_when_unset() {
    for (label, emitted) in [
        (
            "ClientCapabilities",
            serde_json::to_value(ClientCapabilities::default()),
        ),
        (
            "ServerCapabilities",
            serde_json::to_value(ServerCapabilities::default()),
        ),
    ] {
        let emitted = emitted.expect("default capabilities serialize to JSON");
        for field in ["experimental", "extensions"] {
            assert!(
                emitted.get(field).is_none(),
                "{label} emitted `{field}` while unset; skip_serializing_if is what keeps this an \
                 additive change, and emitting null here alters the bytes every existing peer sends"
            );
        }
    }
}
