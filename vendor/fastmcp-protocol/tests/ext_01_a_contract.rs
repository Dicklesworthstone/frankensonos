//! Literal frozen EXT-01 A runner entries.

use std::collections::BTreeMap;

use fastmcp_core::sha256_bounded;
use fastmcp_protocol::{
    ClientExtensionDiscovery, ExtensionDescriptor, ExtensionDescriptorRegistry, ExtensionDirection,
    ExtensionFallbackPolicy, ExtensionHttpEraDisposition, ExtensionId, ExtensionMethodDescriptor,
    ExtensionNegotiationResolver, ExtensionRegistryError, ExtensionRoutingHeaderDescriptor,
    ExtensionSettings, ExtensionSettingsSchema, ServerExtensionDiscovery,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

fn descriptor(id: ExtensionId, method: &str) -> ExtensionDescriptor {
    ExtensionDescriptor {
        id,
        client_settings: ExtensionSettingsSchema {
            schema_id: "client-v1".into(),
            codec_id: "client-codec-v1".into(),
        },
        server_settings: ExtensionSettingsSchema {
            schema_id: "server-v1".into(),
            codec_id: "server-codec-v1".into(),
        },
        resolver: ExtensionNegotiationResolver {
            id: "compatible-v1".into(),
            version: 1,
            fallback: ExtensionFallbackPolicy::RejectOneSided,
        },
        method: Some(ExtensionMethodDescriptor {
            name: method.into(),
            direction: ExtensionDirection::ClientToServer,
            http_era_disposition: Some(ExtensionHttpEraDisposition::ModernExclusive),
            legacy_fallback: false,
        }),
        notification: None,
        result_discriminator: Some("com.example/result".into()),
        routing_headers: vec![ExtensionRoutingHeaderDescriptor {
            name: "Mcp-Example".into(),
        }],
        stdio_correlation: None,
    }
}

#[derive(Deserialize)]
struct TypedSettings {
    nested: Value,
    enabled: bool,
}

#[test]
fn ext_01_a_positive() {
    let id = ExtensionId::parse("com.example/")
        .expect("prefixed empty name is valid and byte-preserved");
    assert_eq!(id.as_str(), "com.example/");
    assert!(ExtensionId::parse("com.example.mcp/weather").is_ok());

    let mut settings_object = Map::new();
    settings_object.insert("enabled".into(), json!(true));
    settings_object.insert(
        "nested".into(),
        json!({"items": [null, 1.25, {"deep": null}]}),
    );
    let settings = ExtensionSettings::new(Value::Object(settings_object))
        .expect("object settings are preserved");
    let typed: TypedSettings = settings
        .decode()
        .expect("registered descriptor codec decodes typed settings");
    assert!(typed.enabled);
    assert_eq!(typed.nested, json!({"items": [null, 1.25, {"deep": null}]}));
    let empty =
        ExtensionSettings::new(json!({})).expect("empty object is support without settings");

    let mut registry = ExtensionDescriptorRegistry::new();
    registry
        .register(descriptor(id.clone(), "com.example/weather"))
        .expect("registered descriptor is accepted");
    let receipt = registry
        .freeze()
        .expect("registry freezes to a public digest receipt");
    assert_eq!(receipt.descriptor_count(), 1);
    let canonical = r#"[\"fastmcp.ext-01.descriptor-registry.v1\",[{\"clientCodec\":\"client-codec-v1\",\"clientSchema\":\"client-v1\",\"id\":\"com.example/\",\"method\":[\"com.example/weather\",\"ClientToServer\",\"ModernExclusive\",false],\"notification\":null,\"resolver\":[\"compatible-v1\",1,\"RejectOneSided\"],\"resultDiscriminator\":\"com.example/result\",\"routingHeaders\":[\"Mcp-Example\"],\"serverCodec\":\"server-codec-v1\",\"serverSchema\":\"server-v1\",\"stdio\":null}]]"#;
    assert_eq!(
        receipt.digest(),
        sha256_bounded(canonical.as_bytes(), canonical.len())
            .expect("bounded canonical subject")
            .as_bytes()
    );

    let mut client = ClientExtensionDiscovery::default();
    client.extensions.insert(id.clone(), settings.clone());
    let mut server = ServerExtensionDiscovery::default();
    server.extensions.insert(id.clone(), empty);
    assert_eq!(client.extensions.len(), 1);
    assert_eq!(server.extensions.len(), 1);
    let unknown =
        ExtensionId::parse("org.example/diagnostic").expect("nonreserved peer identifier");
    let preserved =
        registry.preserve_unknown_peer_extensions(BTreeMap::from([(unknown.clone(), settings)]));
    assert_eq!(
        preserved
            .get(&unknown)
            .expect("unknown remains diagnostic peer data")
            .as_object()["nested"],
        json!({"items": [null, 1.25, {"deep": null}]})
    );
}

#[test]
fn ext_01_a_planted_negative() {
    let baseline_id = ExtensionId::parse("com.example/").expect("baseline identifier");
    let baseline = descriptor(baseline_id.clone(), "com.example/weather");
    let mut registry = ExtensionDescriptorRegistry::new();
    registry
        .register(baseline.clone())
        .expect("baseline accepts");
    let baseline_digest = registry.freeze().expect("baseline freezes");

    let mutated_id = ExtensionId::parse("com.mcp.tools/")
        .expect_err("changing only second DNS label to mcp is forbidden");
    assert_eq!(
        mutated_id,
        ExtensionRegistryError::ReservedNamespace("com.mcp.tools/".into())
    );
    assert_eq!(baseline.id, baseline_id);
    assert_eq!(
        registry
            .receipt()
            .expect("rejected candidate cannot mutate frozen state"),
        &baseline_digest
    );

    let mut fresh = ExtensionDescriptorRegistry::new();
    fresh
        .register(baseline)
        .expect("fresh baseline reaccepts after the one-field negative");
    assert_eq!(
        fresh.freeze().expect("fresh baseline digest"),
        baseline_digest
    );
}
