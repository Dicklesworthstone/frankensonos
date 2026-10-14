//! Literal frozen runner entries for PXY-LEG-01 implementation A.

use fastmcp_core::CanonicalHttpUrl;
use fastmcp_protocol::protocol_policy::{
    HttpModernProbe, HttpProbeBody, ProtocolEra, ProtocolPolicy, StdioOpeningFrame,
};
use fastmcp_server::ProxyClient;

fn url(value: &str) -> CanonicalHttpUrl {
    CanonicalHttpUrl::parse(value).expect("configured HTTP target is canonical")
}

#[test]
fn pxy_leg_01_a_positive() {
    let mut bindings = ProxyClient::upstream_binding_registry();

    let modern_stdio = bindings
        .bind_stdio(
            "route-modern-stdio",
            "stdio:peer-a",
            "leg-03-receipt-a",
            7,
            ProtocolPolicy::Auto,
            StdioOpeningFrame::ModernRequest {
                protocol_version: "2026-07-28".to_owned(),
            },
        )
        .expect("Auto selects exact modern stdio independently");
    assert_eq!(modern_stdio.era(), ProtocolEra::Modern2026);
    assert_eq!(modern_stdio.policy(), ProtocolPolicy::Auto);
    assert_eq!(modern_stdio.configuration_generation(), 7);

    let legacy_stdio = bindings
        .bind_stdio(
            "route-legacy-stdio",
            "stdio:peer-b",
            "leg-03-receipt-b",
            7,
            ProtocolPolicy::Auto,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect("Auto selects exact legacy stdio independently");
    assert_eq!(legacy_stdio.era(), ProtocolEra::Legacy2024);
    assert_eq!(legacy_stdio.policy(), ProtocolPolicy::Auto);
    assert!(legacy_stdio.uses_legacy_adapter());
    assert!(!legacy_stdio.uses_http_transport());
    assert_eq!(modern_stdio.era(), ProtocolEra::Modern2026);

    let legacy_only = bindings
        .bind_stdio(
            "route-legacy-only-stdio",
            "stdio:peer-e",
            "leg-03-receipt-e",
            9,
            ProtocolPolicy::LegacyOnly,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect("LegacyOnly binds exact legacy stdio directly");
    assert_eq!(legacy_only.era(), ProtocolEra::Legacy2024);
    assert_eq!(legacy_only.policy(), ProtocolPolicy::LegacyOnly);

    let legacy_http_error = bindings
        .bind_http(
            "route-legacy-http",
            "http:peer-c",
            "leg-http-01-receipt-c",
            8,
            ProtocolPolicy::Auto,
            Some(url("https://modern.example.test/mcp")),
            Some(url("https://legacy.example.test/sse")),
            Some(url("https://legacy.example.test/messages")),
            "partition-c".to_owned(),
            "security-partition-c".to_owned(),
            "http-profile-c".to_owned(),
            3,
            11,
            HttpModernProbe {
                status: 404,
                body: HttpProbeBody::Empty,
            },
        )
        .expect_err("Auto fallback authorization cannot pre-bind exact legacy HTTP");
    assert_eq!(
        legacy_http_error.code,
        fastmcp_core::McpErrorCode::InvalidRequest
    );
    assert!(
        legacy_http_error
            .message
            .contains("before endpoint validation")
    );

    let auto_modern_after_refusal = bindings
        .bind_http(
            "route-legacy-http",
            "http:peer-c",
            "leg-http-01-receipt-c",
            8,
            ProtocolPolicy::Auto,
            Some(url("https://modern.example.test/mcp")),
            Some(url("https://legacy.example.test/sse")),
            Some(url("https://legacy.example.test/messages")),
            "partition-c".to_owned(),
            "security-partition-c".to_owned(),
            "http-profile-c".to_owned(),
            3,
            11,
            HttpModernProbe {
                status: 200,
                body: HttpProbeBody::RecognizedModernJsonRpc,
            },
        )
        .expect("rejected fallback leaves Auto free to bind recognized modern HTTP");
    assert_eq!(auto_modern_after_refusal.era(), ProtocolEra::Modern2026);
    assert_eq!(auto_modern_after_refusal.policy(), ProtocolPolicy::Auto);
    assert!(!auto_modern_after_refusal.uses_legacy_adapter());
    assert!(auto_modern_after_refusal.uses_http_transport());

    let modern_http = bindings
        .bind_http(
            "route-modern-http",
            "http:peer-d",
            "leg-http-01-receipt-d",
            8,
            ProtocolPolicy::ModernOnly,
            Some(url("https://modern.example.test/mcp-2")),
            None,
            None,
            "partition-d".to_owned(),
            "security-partition-d".to_owned(),
            "http-profile-d".to_owned(),
            4,
            12,
            HttpModernProbe {
                status: 200,
                body: HttpProbeBody::RecognizedModernJsonRpc,
            },
        )
        .expect("ModernOnly binds only modern HTTP");
    assert_eq!(modern_http.era(), ProtocolEra::Modern2026);
    assert_eq!(modern_http.policy(), ProtocolPolicy::ModernOnly);
    assert!(!modern_http.uses_legacy_adapter());
    assert!(modern_http.uses_http_transport());

    let cached = bindings
        .bind_stdio(
            "route-modern-stdio",
            "stdio:peer-a",
            "leg-03-receipt-a",
            7,
            ProtocolPolicy::Auto,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect("a selected route cannot be rebound by later peer traffic");
    assert_eq!(cached, modern_stdio);
}

#[test]
fn pxy_leg_01_a_planted_negative() {
    let mut bindings = ProxyClient::upstream_binding_registry();
    let baseline = bindings
        .bind_stdio(
            "route-legacy-stdio",
            "stdio:peer-b",
            "leg-03-receipt-b",
            7,
            ProtocolPolicy::Auto,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect("baseline exact legacy route selects under Auto");

    // One field changes: the immutable policy changes from Auto to ModernOnly.
    let error = bindings
        .bind_stdio(
            "route-legacy-stdio",
            "stdio:peer-b",
            "leg-03-receipt-b",
            7,
            ProtocolPolicy::ModernOnly,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect_err("ModernOnly refuses an exact legacy opening");
    assert_eq!(error.code, fastmcp_core::McpErrorCode::InvalidRequest);

    let unchanged = bindings
        .bind_stdio(
            "route-legacy-stdio",
            "stdio:peer-b",
            "leg-03-receipt-b",
            7,
            ProtocolPolicy::Auto,
            StdioOpeningFrame::LegacyInitialize,
        )
        .expect("rejected policy mutation leaves original route binding intact");
    assert_eq!(unchanged, baseline);
    assert_eq!(unchanged.era(), ProtocolEra::Legacy2024);
}
