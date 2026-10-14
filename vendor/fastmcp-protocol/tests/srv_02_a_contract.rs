use std::collections::BTreeMap;

use fastmcp_protocol::{
    DiscoveryCacheHints, ResultPeerDiagnostic, SERVER_DISCOVER_METHOD,
    SERVER_DISCOVER_SUPPORTED_VERSIONS, ServerBehavior, ServerBehaviorRegistry,
    ServerDiscoverCapabilities, ServerDiscoverRequest, ServerDiscoverResult, ServerInfo,
    ServerInstructions,
};
use serde_json::{Value, json};

/// Projects one behavior registry through the same public construction path the
/// positive uses, and returns just the advertised capabilities.
fn capability_wire(behaviors: impl IntoIterator<Item = ServerBehavior>) -> Value {
    let registry = ServerBehaviorRegistry::from_behaviors(behaviors);
    let capabilities = ServerDiscoverCapabilities::from_registry(&registry, BTreeMap::new())
        .expect("a bounded registry projects capabilities");
    let result = ServerDiscoverResult::new(
        capabilities,
        ServerInfo {
            name: "contract-server".to_owned(),
            version: "1.0.0".to_owned(),
        },
        None,
        DiscoveryCacheHints::private_ttl_ms(0),
    );
    serde_json::to_value(&result).expect("variant result encodes")["capabilities"].clone()
}

#[test]
fn srv_02_a_positive() {
    let registry = ServerBehaviorRegistry::from_behaviors([
        ServerBehavior::LoggingRequestEmitter,
        ServerBehavior::CompletionComplete,
        ServerBehavior::ToolsList,
        ServerBehavior::ToolsListChangedNotification,
        ServerBehavior::ResourcesList,
        ServerBehavior::ResourcesListChangedNotification,
        ServerBehavior::ResourcesSubscribe,
        ServerBehavior::SubscriptionsListen,
        ServerBehavior::ResourceUpdateDelivery,
        ServerBehavior::PromptsList,
        ServerBehavior::PromptsListChangedNotification,
    ]);
    let capabilities = ServerDiscoverCapabilities::from_registry(
        &registry,
        BTreeMap::from([("io.fastmcp/example".to_owned(), json!({"enabled": true}))]),
    )
    .expect("bounded installed extension is discoverable");
    let result = ServerDiscoverResult::new(
        capabilities,
        ServerInfo {
            name: "contract-server".to_owned(),
            version: "1.0.0".to_owned(),
        },
        Some(ServerInstructions::new("Use the installed tools.").expect("bounded instructions")),
        DiscoveryCacheHints::private_ttl_ms(60_000),
    );

    let request = serde_json::to_value(ServerDiscoverRequest::default()).expect("request encodes");
    let wire = serde_json::to_value(&result).expect("result encodes");

    assert_eq!(SERVER_DISCOVER_METHOD, "server/discover");
    assert_eq!(
        request,
        json!({
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                "io.modelcontextprotocol/clientCapabilities": {},
            },
        })
    );
    assert_eq!(wire["resultType"], json!("complete"));
    assert_eq!(
        wire["supportedVersions"],
        json!(SERVER_DISCOVER_SUPPORTED_VERSIONS)
    );
    assert!(wire.get("protocolVersions").is_none());
    assert_eq!(
        wire["_meta"]["io.modelcontextprotocol/serverInfo"],
        json!({"name": "contract-server", "version": "1.0.0"})
    );
    assert_eq!(wire["capabilities"]["tools"]["listChanged"], json!(true));
    assert_eq!(wire["capabilities"]["resources"]["subscribe"], json!(true));
    assert_eq!(wire["capabilities"]["prompts"]["listChanged"], json!(true));
    assert!(wire["capabilities"].get("subscriptions").is_none());
    assert_eq!(
        wire["capabilities"]["extensions"]["io.fastmcp/example"]["enabled"],
        json!(true)
    );
    assert_eq!(wire["ttlMs"], json!(60_000));
    assert_eq!(wire["cacheScope"], json!("private"));
    assert_eq!(
        DiscoveryCacheHints::private_ttl_ms(u64::MAX)
            .ttl_ms()
            .try_as_millis(),
        Ok(u64::MAX),
        "the typed cache-hint field admits its complete nonnegative wire domain"
    );

    // REGISTRY VARIANCE. Every assertion above runs against a registry holding
    // EVERY behavior, so together they prove presence-mapping only: a
    // projection that hard-coded this exact capability shape would satisfy all
    // of them. These variants each drop one behavior and require the
    // corresponding capability, or sub-capability, to follow it.
    //
    // This is the same defect shape that made the SRV-02 B advertisement
    // assertions vacuous until they were varied against registration: an
    // assertion that restates a derivation instead of testing it.
    let full = wire["capabilities"].clone();

    // A registry with no behaviors at all must advertise no known capability.
    // Fails against any projection that emits a capability unconditionally.
    let none = capability_wire([]);
    for known in ["tools", "resources", "prompts", "completions", "logging"] {
        assert!(
            none.get(known).is_none(),
            "an empty behavior registry advertised {known}: {none}"
        );
    }
    assert_ne!(
        none, full,
        "the capability projection does not track the registry at all"
    );

    // tools/list WITHOUT its notification producer. The parent capability must
    // appear and its listChanged sub-capability must not be true: each
    // sub-capability tracks its OWN behavior, not merely the parent's presence.
    let tools_without_notifications = capability_wire([ServerBehavior::ToolsList]);
    assert!(
        tools_without_notifications.get("tools").is_some(),
        "ToolsList must still advertise the tools capability: {tools_without_notifications}"
    );
    assert_ne!(
        tools_without_notifications["tools"]["listChanged"],
        json!(true),
        "listChanged must track its own notification producer rather than the \
         parent capability: {tools_without_notifications}"
    );

    // resources WITHOUT subscribe. `resources.subscribe: true` is gated on
    // subscriptions/listen being able to deliver individual resource updates,
    // so a registry lacking that must not advertise it.
    let resources_without_subscribe = capability_wire([
        ServerBehavior::ResourcesList,
        ServerBehavior::ResourcesListChangedNotification,
    ]);
    assert!(
        resources_without_subscribe.get("resources").is_some(),
        "ResourcesList must still advertise the resources capability: \
         {resources_without_subscribe}"
    );
    assert_ne!(
        resources_without_subscribe["resources"]["subscribe"],
        json!(true),
        "subscribe must track its own delivery behavior: {resources_without_subscribe}"
    );

    // Prompts absent while tools are present, so the two axes are independent
    // rather than one flag turning on every known capability at once.
    assert!(
        tools_without_notifications.get("prompts").is_none(),
        "a tools-only registry advertised prompts: {tools_without_notifications}"
    );

    let mut multi_version_peer = wire;
    multi_version_peer["supportedVersions"] = json!(["2024-11-05", "2026-07-28"]);
    let decoded: ServerDiscoverResult = serde_json::from_value(multi_version_peer)
        .expect("final discovery accepts every schema-valid string version array");
    assert_eq!(
        decoded
            .supported_versions()
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        ["2024-11-05", "2026-07-28"],
        "version selection remains the negotiation layer's responsibility"
    );
}

#[test]
fn srv_02_a_planted_negative() {
    let capabilities = ServerDiscoverCapabilities::from_registry(
        &ServerBehaviorRegistry::from_behaviors([ServerBehavior::ToolsList]),
        BTreeMap::new(),
    )
    .expect("empty extension registry is valid");
    let admitted = ServerDiscoverResult::new(
        capabilities,
        ServerInfo {
            name: "contract-server".to_owned(),
            version: "1.0.0".to_owned(),
        },
        None,
        DiscoveryCacheHints::private_ttl_ms(0),
    );
    let unchanged_before = serde_json::to_vec(&admitted).expect("admitted state encodes");
    let mut planted: Value = serde_json::to_value(&admitted).expect("baseline result encodes");
    planted
        .as_object_mut()
        .expect("baseline result is an object")
        .remove("resultType");

    let compatibility = serde_json::from_value::<ServerDiscoverResult>(planted.clone())
        .expect("an otherwise-valid absent resultType defaults to complete at client ingestion");
    assert_eq!(compatibility.result_type(), "complete");
    assert_eq!(
        compatibility.peer_diagnostic(),
        Some(ResultPeerDiagnostic::ModernMissingResultType)
    );
    let compatibility_wire =
        serde_json::to_value(compatibility).expect("compatibility result re-encodes");
    let mut canonicalized = planted.clone();
    canonicalized
        .as_object_mut()
        .expect("planted result is an object")
        .insert("resultType".to_owned(), json!("complete"));
    assert_eq!(
        compatibility_wire, canonicalized,
        "compatibility evidence remains diagnostic while final emission restores the discriminator"
    );
    assert_ne!(
        compatibility_wire, planted,
        "a peer omission must never make locally emitted final discovery omit resultType"
    );
    assert_eq!(
        serde_json::to_vec(&admitted).expect("admitted state still encodes"),
        unchanged_before,
        "compatibility admission cannot mutate an already-authored result"
    );

    let mut explicit_null: Value =
        serde_json::to_value(&admitted).expect("baseline result re-encodes");
    explicit_null["resultType"] = Value::Null;
    assert!(
        serde_json::from_value::<ServerDiscoverResult>(explicit_null).is_err(),
        "explicit null is not the absent-discriminator compatibility case"
    );

    let mut wrong_type: Value =
        serde_json::to_value(&admitted).expect("baseline result re-encodes");
    wrong_type["resultType"] = json!({ "complete": true });
    assert!(
        serde_json::from_value::<ServerDiscoverResult>(wrong_type).is_err(),
        "a non-string discriminator is invalid rather than absent"
    );
}
