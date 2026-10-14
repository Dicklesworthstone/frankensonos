//! Frozen FND-03 B public transport-era classification harnesses.

use fastmcp_core::CanonicalHttpUrl;
use fastmcp_protocol::protocol_policy::{
    HttpEndpointBundle, HttpEndpointBundleError, HttpEraCache, HttpEraDecision, HttpModernProbe,
    HttpProbeBody, ModernVersionSupport, ProtocolEra, ProtocolPolicy, ProtocolVersion,
    ProtocolVersionError, StdioEraClassifier, StdioEraDecision, StdioEraRejection,
    StdioEraState, StdioOpeningFrame,
};

fn auto_bundle(security_partition: &str, policy_generation: u64) -> HttpEndpointBundle {
    HttpEndpointBundle::new(
        ProtocolPolicy::Auto,
        Some(CanonicalHttpUrl::parse("https://api.example.test/mcp?tenant=alpha").unwrap()),
        Some(CanonicalHttpUrl::parse("https://api.example.test/sse?tenant=alpha").unwrap()),
        Some(CanonicalHttpUrl::parse("https://api.example.test/messages?tenant=alpha").unwrap()),
        "credential-partition-a".to_owned(),
        security_partition.to_owned(),
        "http-sse-v2".to_owned(),
        policy_generation,
        7,
        11,
    )
    .expect("complete Auto bundle must be admitted")
}

#[test]
fn fnd_03_b_positive() {
    assert_eq!(
        ProtocolVersion::parse("2026-07-28"),
        Ok(ProtocolVersion::MODERN_2026)
    );
    assert_eq!(
        ProtocolVersion::parse("2024-11-05"),
        Ok(ProtocolVersion::LEGACY_2024)
    );
    assert_eq!(
        ProtocolVersion::parse("2025-11-25"),
        Err(ProtocolVersionError::UnsupportedVersion {
            received: "2025-11-25".to_owned(),
        })
    );

    let mut modern_stdio = StdioEraClassifier::new(ProtocolPolicy::Auto);
    assert_eq!(
        modern_stdio.classify_opening(StdioOpeningFrame::ModernRequest {
            protocol_version: "2026-07-28".to_owned(),
        }),
        StdioEraDecision::Selected {
            era: ProtocolEra::Modern2026,
            modern_version: Some(ModernVersionSupport::Supported),
        }
    );
    assert_eq!(
        modern_stdio.state(),
        &StdioEraState::Selected(ProtocolEra::Modern2026)
    );

    let mut legacy_stdio = StdioEraClassifier::new(ProtocolPolicy::Auto);
    assert_eq!(
        legacy_stdio.classify_opening(StdioOpeningFrame::LegacyInitialize),
        StdioEraDecision::Selected {
            era: ProtocolEra::Legacy2024,
            modern_version: None,
        }
    );

    let security_a = auto_bundle("security-partition-a", 3);
    let security_b = auto_bundle("security-partition-b", 3);
    let regenerated_policy = auto_bundle("security-partition-a", 4);
    assert_ne!(security_a.key(), security_b.key());
    assert_ne!(security_a.key(), regenerated_policy.key());

    let mut cache = HttpEraCache::default();
    assert_eq!(
        cache.classify_or_cached(
            &security_a,
            HttpModernProbe {
                status: 500,
                body: HttpProbeBody::RecognizedModernJsonRpc,
            },
        ),
        HttpEraDecision::Selected(ProtocolEra::Modern2026)
    );
    assert_eq!(
        cache.classify_or_cached(
            &security_a,
            HttpModernProbe {
                status: 404,
                body: HttpProbeBody::Empty,
            },
        ),
        HttpEraDecision::Selected(ProtocolEra::Modern2026),
        "an already-modern bundle must not retry or downgrade"
    );
    assert_eq!(
        cache.classify_or_cached(
            &security_b,
            HttpModernProbe {
                status: 404,
                body: HttpProbeBody::Empty,
            },
        ),
        HttpEraDecision::LegacySseFallbackAuthorized
    );
    assert_eq!(
        cache.selected_era(&security_a.key()),
        Some(ProtocolEra::Modern2026)
    );
    assert_eq!(cache.selected_era(&security_b.key()), None);
}

#[test]
fn fnd_03_b_planted_negative() {
    // The planted dimension is the opening-frame variant, and nothing else.
    // Both twins carry the identical protocol-version spelling, the identical
    // policy, and an identically constructed classifier; the accepted frame
    // is a plain modern request and the planted frame is the adversarial one
    // that asserts BOTH eras at once.
    //
    // Determinism only means something if ambiguity is refused rather than
    // resolved to a default, so the planted input must reach a typed refusal
    // and close the process, not select an era.
    const SHARED_VERSION: &str = "2026-07-28";

    let accepted_frame = StdioOpeningFrame::ModernRequest {
        protocol_version: SHARED_VERSION.to_owned(),
    };
    let planted_frame = StdioOpeningFrame::MixedInitializeAndModernMetadata {
        protocol_version: SHARED_VERSION.to_owned(),
    };

    let mut accepted = StdioEraClassifier::new(ProtocolPolicy::Auto);
    assert_eq!(
        accepted.classify_opening(accepted_frame),
        StdioEraDecision::Selected {
            era: ProtocolEra::Modern2026,
            modern_version: Some(ModernVersionSupport::Supported),
        }
    );
    let accepted_state = accepted.state().clone();

    let mut rejected = StdioEraClassifier::new(ProtocolPolicy::Auto);
    assert_eq!(
        rejected.classify_opening(planted_frame),
        StdioEraDecision::RejectedAndClosed {
            reason: StdioEraRejection::MixedEraMarkers,
        },
        "an opening frame claiming both eras must be refused, never resolved \
         to a default era"
    );

    // Refused means closed: no era was selected, and the process cannot be
    // asked again. A classifier that re-ran here would let a peer retry until
    // it got the era it wanted.
    assert_eq!(rejected.state(), &StdioEraState::TerminalWithoutEra);
    assert_eq!(
        rejected.classify_opening(StdioOpeningFrame::ModernRequest {
            protocol_version: SHARED_VERSION.to_owned(),
        }),
        StdioEraDecision::AlreadyTerminal,
        "a terminal process must not reclassify"
    );
    assert_eq!(
        rejected.classify_opening(StdioOpeningFrame::LegacyInitialize),
        StdioEraDecision::AlreadyTerminal
    );
    assert_eq!(rejected.state(), &StdioEraState::TerminalWithoutEra);

    // Unchanged accepted state: the refusal contaminated nothing.
    assert_eq!(accepted.state(), &accepted_state);
    assert_eq!(
        accepted.state(),
        &StdioEraState::Selected(ProtocolEra::Modern2026)
    );

    // The same one-variable discipline on the HTTP side: the two probes differ
    // only in their status, and only the frozen downgrade statuses may permit
    // a legacy observation.
    let bundle = auto_bundle("security-partition-negative", 3);

    let mut permitted = HttpEraCache::default();
    assert_eq!(
        permitted.classify_or_cached(
            &bundle,
            HttpModernProbe {
                status: 404,
                body: HttpProbeBody::Empty,
            },
        ),
        HttpEraDecision::LegacySseFallbackAuthorized
    );

    let mut refused = HttpEraCache::default();
    assert_eq!(
        refused.classify_or_cached(
            &bundle,
            HttpModernProbe {
                status: 500,
                body: HttpProbeBody::Empty,
            },
        ),
        HttpEraDecision::RejectedWithoutLegacyFallback,
        "a status outside the frozen downgrade set must not authorize legacy GET"
    );

    // Neither outcome is an era selection, so neither may be cached. A cached
    // fallback authorization would become a silent downgrade on the next call.
    assert_eq!(permitted.selected_era(&bundle.key()), None);
    assert_eq!(refused.selected_era(&bundle.key()), None);
}

/// Every ambiguous or adversarial stdio opening is refused and closes the
/// process; none of them resolves to a default era.
#[test]
fn fnd_03_b_ambiguous_stdio_openings_refuse_and_never_retry() {
    let refusals = [
        (
            StdioOpeningFrame::MixedInitializeAndModernMetadata {
                protocol_version: "2026-07-28".to_owned(),
            },
            StdioEraRejection::MixedEraMarkers,
        ),
        (
            StdioOpeningFrame::Notification,
            StdioEraRejection::NotificationCannotClassify,
        ),
        (
            StdioOpeningFrame::Response,
            StdioEraRejection::ResponseCannotClassify,
        ),
        (
            StdioOpeningFrame::Malformed,
            StdioEraRejection::MalformedOpeningFrame,
        ),
    ];

    for (frame, reason) in refusals {
        let mut classifier = StdioEraClassifier::new(ProtocolPolicy::Auto);
        assert_eq!(
            classifier.classify_opening(frame.clone()),
            StdioEraDecision::RejectedAndClosed { reason },
            "{frame:?} must be refused under Auto"
        );
        assert_eq!(
            classifier.state(),
            &StdioEraState::TerminalWithoutEra,
            "{frame:?} must not leave a selected era"
        );
        assert_eq!(
            classifier.classify_opening(StdioOpeningFrame::ModernRequest {
                protocol_version: "2026-07-28".to_owned(),
            }),
            StdioEraDecision::AlreadyTerminal,
            "{frame:?} must not permit a second classification attempt"
        );
    }
}

/// A fixed policy never negotiates: ModernOnly refuses legacy traffic without
/// falling back, and LegacyOnly refuses modern discovery.
#[test]
fn fnd_03_b_fixed_policies_never_fall_back() {
    let mut modern_only = StdioEraClassifier::new(ProtocolPolicy::ModernOnly);
    assert_eq!(
        modern_only.state(),
        &StdioEraState::Selected(ProtocolEra::Modern2026),
        "ModernOnly is pinned before the first frame, never negotiated"
    );
    assert_eq!(
        modern_only.classify_opening(StdioOpeningFrame::LegacyInitialize),
        StdioEraDecision::RejectedUnderSelectedEra {
            era: ProtocolEra::Modern2026,
            reason: StdioEraRejection::CrossEraTraffic,
        }
    );

    let mut legacy_only = StdioEraClassifier::new(ProtocolPolicy::LegacyOnly);
    assert_eq!(
        legacy_only.state(),
        &StdioEraState::Selected(ProtocolEra::Legacy2024)
    );
    assert_eq!(
        legacy_only.classify_opening(StdioOpeningFrame::ModernRequest {
            protocol_version: "2026-07-28".to_owned(),
        }),
        StdioEraDecision::RejectedUnderSelectedEra {
            era: ProtocolEra::Legacy2024,
            reason: StdioEraRejection::CrossEraTraffic,
        }
    );

    // ModernOnly over HTTP refuses without authorizing any legacy observation,
    // whatever the probe says.
    let modern_bundle = HttpEndpointBundle::new(
        ProtocolPolicy::ModernOnly,
        Some(CanonicalHttpUrl::parse("https://api.example.test/mcp").unwrap()),
        None,
        None,
        "credential-partition-a".to_owned(),
        "security-partition-modern-only".to_owned(),
        "http-sse-v2".to_owned(),
        3,
        7,
        11,
    )
    .expect("ModernOnly bundle with only a modern target must be admitted");

    let mut cache = HttpEraCache::default();
    for probe in [
        HttpModernProbe { status: 404, body: HttpProbeBody::Empty },
        HttpModernProbe { status: 405, body: HttpProbeBody::Unrecognized },
        HttpModernProbe { status: 200, body: HttpProbeBody::TransportFailure },
    ] {
        assert_eq!(
            cache.classify_or_cached(&modern_bundle, probe),
            HttpEraDecision::RejectedWithoutLegacyFallback,
            "ModernOnly must never authorize a legacy fallback: {probe:?}"
        );
    }
    assert_eq!(cache.selected_era(&modern_bundle.key()), None);
}

/// A recognized modern JSON-RPC body fixes Modern at any status, and only the
/// frozen downgrade statuses with an empty or unrecognized body permit one
/// legacy observation.
#[test]
fn fnd_03_b_http_downgrade_matrix_is_exact() {
    let bundle = auto_bundle("security-partition-matrix", 3);

    // A recognized modern body fixes Modern regardless of status.
    for status in [200, 400, 404, 405, 500, 503] {
        let mut cache = HttpEraCache::default();
        assert_eq!(
            cache.classify_or_cached(
                &bundle,
                HttpModernProbe {
                    status,
                    body: HttpProbeBody::RecognizedModernJsonRpc,
                },
            ),
            HttpEraDecision::Selected(ProtocolEra::Modern2026),
            "status {status} with a recognized modern body must fix Modern"
        );
    }

    // Only 400/404/405 with an empty or unrecognized body may permit a legacy
    // observation; everything else refuses.
    for status in [200, 201, 301, 401, 403, 406, 500, 502, 503] {
        for body in [HttpProbeBody::Empty, HttpProbeBody::Unrecognized] {
            let mut cache = HttpEraCache::default();
            assert_eq!(
                cache.classify_or_cached(&bundle, HttpModernProbe { status, body }),
                HttpEraDecision::RejectedWithoutLegacyFallback,
                "status {status} with {body:?} must not authorize legacy GET"
            );
        }
    }
    for status in [400, 404, 405] {
        for body in [HttpProbeBody::Empty, HttpProbeBody::Unrecognized] {
            let mut cache = HttpEraCache::default();
            assert_eq!(
                cache.classify_or_cached(&bundle, HttpModernProbe { status, body }),
                HttpEraDecision::LegacySseFallbackAuthorized,
                "status {status} with {body:?} must permit one legacy observation"
            );
        }
    }

    // A transport failure is not a downgrade signal at any status.
    for status in [400, 404, 405, 500] {
        let mut cache = HttpEraCache::default();
        assert_eq!(
            cache.classify_or_cached(
                &bundle,
                HttpModernProbe {
                    status,
                    body: HttpProbeBody::TransportFailure,
                },
            ),
            HttpEraDecision::RejectedWithoutLegacyFallback,
            "a transport failure at {status} must not authorize legacy GET"
        );
    }
}

/// A structurally modern request carrying an unsupported version still selects
/// Modern exactly once and reports the unsupported spelling verbatim; it never
/// downgrades and never normalizes into a supported era.
#[test]
fn fnd_03_b_unsupported_version_selects_modern_without_downgrading() {
    let mut classifier = StdioEraClassifier::new(ProtocolPolicy::Auto);
    assert_eq!(
        classifier.classify_opening(StdioOpeningFrame::ModernRequest {
            protocol_version: "2025-11-25".to_owned(),
        }),
        StdioEraDecision::Selected {
            era: ProtocolEra::Modern2026,
            modern_version: Some(ModernVersionSupport::Unsupported {
                received: "2025-11-25".to_owned(),
            }),
        }
    );
    assert_eq!(
        classifier.state(),
        &StdioEraState::Selected(ProtocolEra::Modern2026)
    );
    // Having selected Modern, legacy traffic is cross-era and is refused.
    assert_eq!(
        classifier.classify_opening(StdioOpeningFrame::LegacyInitialize),
        StdioEraDecision::RejectedUnderSelectedEra {
            era: ProtocolEra::Modern2026,
            reason: StdioEraRejection::CrossEraTraffic,
        }
    );
}

/// Every dimension of the bundle key discriminates, and origin equality alone
/// is never bundle identity.
///
/// The key binds ten fields. Before this test only two of them were ever
/// varied, so eight could have been dropped from the key without any test
/// noticing — and a dropped field is a cache collision between two bundles
/// that differ in exactly that field. Two of those fields are the credential
/// and security partitions, so a collision there would let one partition's
/// negotiated era be served to another.
#[test]
fn fnd_03_b_bundle_key_binds_every_dimension() {
    /// One bundle with every key dimension supplied explicitly.
    #[allow(clippy::too_many_arguments)]
    fn bundle_with_policy(
        policy: ProtocolPolicy,
        modern: &str,
        sse: &str,
        message: &str,
        credential_partition: &str,
        security_partition: &str,
        transport_profile: &str,
        policy_generation: u64,
        configuration_generation: u64,
        legacy_receipt_generation: u64,
    ) -> HttpEndpointBundle {
        HttpEndpointBundle::new(
            policy,
            Some(CanonicalHttpUrl::parse(modern).unwrap()),
            Some(CanonicalHttpUrl::parse(sse).unwrap()),
            Some(CanonicalHttpUrl::parse(message).unwrap()),
            credential_partition.to_owned(),
            security_partition.to_owned(),
            transport_profile.to_owned(),
            policy_generation,
            configuration_generation,
            legacy_receipt_generation,
        )
        .expect("a complete bundle must be admitted")
    }

    /// The same, pinned to Auto, for the nine non-policy dimensions.
    #[allow(clippy::too_many_arguments)]
    fn bundle(
        modern: &str,
        sse: &str,
        message: &str,
        credential_partition: &str,
        security_partition: &str,
        transport_profile: &str,
        policy_generation: u64,
        configuration_generation: u64,
        legacy_receipt_generation: u64,
    ) -> HttpEndpointBundle {
        bundle_with_policy(
            ProtocolPolicy::Auto,
            modern,
            sse,
            message,
            credential_partition,
            security_partition,
            transport_profile,
            policy_generation,
            configuration_generation,
            legacy_receipt_generation,
        )
    }

    const MODERN: &str = "https://api.example.test/mcp?tenant=alpha";
    const SSE: &str = "https://api.example.test/sse?tenant=alpha";
    const MESSAGE: &str = "https://api.example.test/messages?tenant=alpha";

    let baseline = bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 7, 11);

    // One variant per key dimension, each differing from the baseline in
    // exactly that dimension and nothing else.
    let variants: [(&str, HttpEndpointBundle); 9] = [
        (
            "modern_post_target",
            bundle("https://api.example.test/mcp2?tenant=alpha", SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 7, 11),
        ),
        (
            "legacy_sse_target",
            bundle(MODERN, "https://api.example.test/sse2?tenant=alpha", MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 7, 11),
        ),
        (
            "legacy_message_post_target",
            bundle(MODERN, SSE, "https://api.example.test/messages2?tenant=alpha", "cred-a", "sec-a", "http-sse-v2", 3, 7, 11),
        ),
        (
            "credential_partition",
            bundle(MODERN, SSE, MESSAGE, "cred-b", "sec-a", "http-sse-v2", 3, 7, 11),
        ),
        (
            "security_partition",
            bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-b", "http-sse-v2", 3, 7, 11),
        ),
        (
            "transport_profile",
            bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v3", 3, 7, 11),
        ),
        (
            "policy_generation",
            bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 4, 7, 11),
        ),
        (
            "configuration_generation",
            bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 8, 11),
        ),
        (
            "legacy_receipt_generation",
            bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 7, 12),
        ),
    ];

    for (dimension, variant) in &variants {
        assert_ne!(
            baseline.key(),
            variant.key(),
            "{dimension} must discriminate the bundle key"
        );
    }

    // The tenth dimension is the policy itself. `new` requires targets but
    // never rejects extra ones, so this variant supplies all three exactly as
    // the baseline does and differs in the policy field alone. Dropping the
    // legacy targets here would also have produced a distinct key, but for
    // three reasons at once, which would not have isolated the policy.
    let modern_only = bundle_with_policy(
        ProtocolPolicy::ModernOnly,
        MODERN,
        SSE,
        MESSAGE,
        "cred-a",
        "sec-a",
        "http-sse-v2",
        3,
        7,
        11,
    );
    assert_ne!(
        baseline.key(),
        modern_only.key(),
        "policy alone must discriminate the bundle key"
    );

    // Every variant is distinct from every other, not merely from the
    // baseline: two dimensions must not alias onto one another.
    let mut keys: Vec<_> = variants.iter().map(|(_, b)| b.key()).collect();
    keys.push(baseline.key());
    keys.push(modern_only.key());
    let total = keys.len();
    let unique: std::collections::HashSet<_> = keys.into_iter().collect();
    assert_eq!(unique.len(), total, "two key dimensions alias onto one another");

    // Origin equality alone is never bundle identity: same scheme, host and
    // port, differing only in path, then only in query.
    let same_origin_different_path = bundle(
        "https://api.example.test/other?tenant=alpha",
        SSE,
        MESSAGE,
        "cred-a",
        "sec-a",
        "http-sse-v2",
        3,
        7,
        11,
    );
    let same_origin_different_query = bundle(
        "https://api.example.test/mcp?tenant=beta",
        SSE,
        MESSAGE,
        "cred-a",
        "sec-a",
        "http-sse-v2",
        3,
        7,
        11,
    );
    assert_ne!(baseline.key(), same_origin_different_path.key());
    assert_ne!(baseline.key(), same_origin_different_query.key());
    assert_ne!(
        same_origin_different_path.key(),
        same_origin_different_query.key()
    );

    // An identically configured bundle is the same bundle: the key is a
    // function of its inputs, not of construction identity. Without this the
    // inequality assertions above would pass for a key that is simply always
    // unique, which would cache nothing and prove nothing.
    let rebuilt = bundle(MODERN, SSE, MESSAGE, "cred-a", "sec-a", "http-sse-v2", 3, 7, 11);
    assert_eq!(baseline.key(), rebuilt.key());
}

/// A negotiated era is never served across a partition boundary.
#[test]
fn fnd_03_b_cache_does_not_leak_across_partitions() {
    let partition_a = auto_bundle("security-partition-leak-a", 3);
    let partition_b = auto_bundle("security-partition-leak-b", 3);

    let mut cache = HttpEraCache::default();
    assert_eq!(
        cache.classify_or_cached(
            &partition_a,
            HttpModernProbe {
                status: 200,
                body: HttpProbeBody::RecognizedModernJsonRpc,
            },
        ),
        HttpEraDecision::Selected(ProtocolEra::Modern2026)
    );

    // Partition B has negotiated nothing, so it must classify on its own
    // probe rather than inherit A's selection.
    assert_eq!(cache.selected_era(&partition_a.key()), Some(ProtocolEra::Modern2026));
    assert_eq!(cache.selected_era(&partition_b.key()), None);
    assert_eq!(
        cache.classify_or_cached(
            &partition_b,
            HttpModernProbe {
                status: 404,
                body: HttpProbeBody::Empty,
            },
        ),
        HttpEraDecision::LegacySseFallbackAuthorized,
        "partition B must not inherit partition A's negotiated era"
    );

    // Invalidating one partition leaves the other untouched.
    assert_eq!(
        cache.invalidate(&partition_a.key()),
        Some(ProtocolEra::Modern2026)
    );
    assert_eq!(cache.selected_era(&partition_a.key()), None);
    assert_eq!(cache.invalidate(&partition_b.key()), None);
}

#[test]
fn http_endpoint_bundle_errors_have_stable_display_and_error_surfaces() {
    let modern = CanonicalHttpUrl::parse("https://api.example.test/mcp").unwrap();
    let legacy_sse = CanonicalHttpUrl::parse("https://api.example.test/sse").unwrap();
    let legacy_message = CanonicalHttpUrl::parse("https://api.example.test/messages").unwrap();

    let errors = [
        (
            HttpEndpointBundle::new(
                ProtocolPolicy::Auto,
                None,
                Some(legacy_sse.clone()),
                Some(legacy_message.clone()),
                "credential-partition-a".to_owned(),
                "security-partition-a".to_owned(),
                "http-sse-v2".to_owned(),
                3,
                7,
                11,
            )
            .expect_err("Auto policy without modern target must be rejected"),
            "protocol policy auto requires a configured modern MCP POST target",
        ),
        (
            HttpEndpointBundle::new(
                ProtocolPolicy::Auto,
                Some(modern.clone()),
                None,
                Some(legacy_message.clone()),
                "credential-partition-a".to_owned(),
                "security-partition-a".to_owned(),
                "http-sse-v2".to_owned(),
                3,
                7,
                11,
            )
            .expect_err("Auto policy without legacy SSE target must be rejected"),
            "protocol policy auto requires a configured legacy SSE GET target",
        ),
        (
            HttpEndpointBundle::new(
                ProtocolPolicy::Auto,
                Some(modern.clone()),
                Some(legacy_sse.clone()),
                None,
                "credential-partition-a".to_owned(),
                "security-partition-a".to_owned(),
                "http-sse-v2".to_owned(),
                3,
                7,
                11,
            )
            .expect_err("Auto policy without legacy message target must be rejected"),
            "protocol policy auto requires a configured legacy message POST target",
        ),
        (
            HttpEndpointBundle::new(
                ProtocolPolicy::ModernOnly,
                Some(CanonicalHttpUrl::parse("https://api.example.test/mcp#fragment").unwrap()),
                None,
                None,
                "credential-partition-a".to_owned(),
                "security-partition-a".to_owned(),
                "http-sse-v2".to_owned(),
                3,
                7,
                11,
            )
            .expect_err("fragment-bearing modern target must be rejected"),
            "configured modern MCP POST target must not contain a fragment",
        ),
        (
            HttpEndpointBundle::new(
                ProtocolPolicy::Auto,
                Some(modern.clone()),
                Some(legacy_sse),
                Some(modern),
                "credential-partition-a".to_owned(),
                "security-partition-a".to_owned(),
                "http-sse-v2".to_owned(),
                3,
                7,
                11,
            )
            .expect_err("same-method canonical target collision must be rejected"),
            "configured modern MCP POST and legacy message POST routes collide at https://api.example.test/mcp",
        ),
    ];

    for (error, expected) in errors {
        let error: &dyn std::error::Error = &error;
        assert_eq!(error.to_string(), expected);
    }

    assert_eq!(
        HttpEndpointBundleError::FragmentNotAllowed {
            route: fastmcp_protocol::protocol_policy::HttpRouteKind::LegacySseGet,
        }
        .to_string(),
        "configured legacy SSE GET target must not contain a fragment"
    );
}
