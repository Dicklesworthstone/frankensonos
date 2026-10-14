//! Frozen top-level PRT-03 acceptance entries.
//!
//! These functions deliberately live in an integration-test crate so the
//! unqualified frozen `--exact` selectors name executable harness entries. The
//! `#[cfg(test)] mod tests` copies inside `src/protocol_version.rs` carry the
//! same function names but are module-qualified (`protocol_version::tests::…`),
//! so `--exact <bare name>` selects these and only these.

use fastmcp_protocol::protocol_version::{
    FINAL_PROTOCOL_VERSION, FinalHttpRequestMetadata, HEADER_MISMATCH_ERROR_CODE,
    HeaderMismatchReason, MCP_METHOD_HEADER, MCP_NAME_HEADER, MCP_PROTOCOL_VERSION_HEADER,
    MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE, MissingRequiredClientCapabilityError,
    FINAL_ADMISSION_PRECEDENCE, FinalAdmissionRule, RequestAdmissionError, RequestVersionMetadata,
    RequiredCapabilitiesError, SUPPORTED_FINAL_PROTOCOL_VERSIONS,
    UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE, admit_final_http_request, admit_final_request,
};
use serde_json::json;

/// A request whose every mirror matches and whose version is supported.
///
/// Every negative below is this value with **one** field changed, so a refusal
/// can only be attributed to that field.
fn admissible() -> FinalHttpRequestMetadata<'static> {
    FinalHttpRequestMetadata {
        version: RequestVersionMetadata {
            header_version: Some(FINAL_PROTOCOL_VERSION),
            body_version: Some(FINAL_PROTOCOL_VERSION),
        },
        header_method: Some("tools/call"),
        body_method: Some("tools/call"),
        header_name: Some("weather"),
        body_name: Some("weather"),
    }
}

/// Returns the header-mismatch reason a request was refused for.
///
/// Panics if the request was admitted, or refused as an unsupported version —
/// conflating those two is the failure this helper exists to make impossible.
fn mismatch_reason(metadata: FinalHttpRequestMetadata<'_>) -> HeaderMismatchReason {
    match admit_final_http_request(metadata) {
        Ok(_) => panic!("this request must not be admitted"),
        Err(RequestAdmissionError::HeaderMismatch(error)) => {
            // Every header mismatch carries the same canonical peer shape:
            // status 400, the frozen code, and deliberately no error data.
            assert_eq!(error.http_status(), 400);
            assert_eq!(error.jsonrpc_error_code(), HEADER_MISMATCH_ERROR_CODE);
            assert_eq!(
                error.canonical_error_data(),
                None,
                "canonical header-mismatch emission must not fabricate `data`"
            );
            error.reason()
        }
        Err(RequestAdmissionError::UnsupportedProtocolVersion(error)) => panic!(
            "a mirror failure must not be classified as unsupported version (requested {:?})",
            error.requested()
        ),
    }
}

#[test]
fn prt_03_a_positive() {
    // The three header names are wire constants; a rename is a protocol break.
    assert_eq!(MCP_PROTOCOL_VERSION_HEADER, "MCP-Protocol-Version");
    assert_eq!(MCP_METHOD_HEADER, "Mcp-Method");
    assert_eq!(MCP_NAME_HEADER, "Mcp-Name");

    // A fully mirrored, supported request is admitted and reports the version.
    let admission =
        admit_final_http_request(admissible()).expect("a fully mirrored final request is admitted");
    assert_eq!(
        admission.protocol_version().as_str(),
        FINAL_PROTOCOL_VERSION
    );
    assert_eq!(FINAL_PROTOCOL_VERSION, "2026-07-28");

    // A method that does not require `Mcp-Name` is admitted without one. The
    // name stage is conditional, and proving admission only ever with a name
    // present would leave that conditionality untested.
    let nameless = admit_final_http_request(FinalHttpRequestMetadata {
        header_method: Some("tools/list"),
        body_method: Some("tools/list"),
        header_name: None,
        body_name: None,
        ..admissible()
    })
    .expect("a method that does not require Mcp-Name needs no name mirror");
    assert_eq!(
        nameless.protocol_version().as_str(),
        FINAL_PROTOCOL_VERSION
    );
    // The complement, which makes the conditionality a real distinction rather
    // than a single observation: the SAME absent name mirror that `tools/list`
    // tolerates is refused for `tools/call`. Without this, "conditional" could
    // be satisfied by a surface that never requires a name at all.
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            header_name: None,
            body_name: None,
            ..admissible()
        }),
        HeaderMismatchReason::MissingNameHeader,
        "a method that requires Mcp-Name must refuse the absent mirror that tools/list accepts"
    );

    // Each version-stage mirror rule reports its own exact reason. These are
    // one-variable changes from `admissible()`, so each reason is attributable.
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: None,
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            ..admissible()
        }),
        HeaderMismatchReason::MissingHeader
    );
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: None,
            },
            ..admissible()
        }),
        HeaderMismatchReason::MissingBodyVersion
    );
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(""),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            ..admissible()
        }),
        HeaderMismatchReason::EmptyHeader
    );
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some(""),
            },
            ..admissible()
        }),
        HeaderMismatchReason::EmptyBodyVersion
    );

    // A well-formed mirror carrying a version this surface does not support is
    // a different error with a different code — not a header mismatch — and it
    // must list the exact supported versions rather than a description of them.
    let unsupported = match admit_final_http_request(FinalHttpRequestMetadata {
        version: RequestVersionMetadata {
            header_version: Some("2025-11-25"),
            body_version: Some("2025-11-25"),
        },
        ..admissible()
    }) {
        Err(RequestAdmissionError::UnsupportedProtocolVersion(error)) => error,
        Ok(_) => panic!("2025-11-25 is not a supported final version"),
        Err(other) => panic!("expected an unsupported-version refusal, got {other:?}"),
    };
    assert_eq!(unsupported.requested(), "2025-11-25");
    assert_eq!(
        unsupported.supported_versions(),
        SUPPORTED_FINAL_PROTOCOL_VERSIONS
    );
    assert_eq!(unsupported.supported_versions(), [FINAL_PROTOCOL_VERSION]);
    assert_eq!(unsupported.http_status(), 400);
    assert_eq!(
        unsupported.jsonrpc_error_code(),
        UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE
    );
    assert_eq!(
        unsupported.canonical_error_data(),
        json!({"supported": [FINAL_PROTOCOL_VERSION], "requested": "2025-11-25"}),
        "the peer is told exactly what is supported and exactly what it sent"
    );
    assert_ne!(
        unsupported.jsonrpc_error_code(),
        HEADER_MISMATCH_ERROR_CODE,
        "an unsupported version and a header mismatch are distinguishable on the wire"
    );
}

#[test]
fn prt_03_a_planted_negative() {
    // Named mutable state, captured before any planted input is offered.
    let baseline = admit_final_http_request(admissible())
        .expect("baseline admissible request")
        .protocol_version()
        .as_str()
        .to_owned();
    let baseline_supported: &[&str] = SUPPORTED_FINAL_PROTOCOL_VERSIONS;

    // Planted mutation 1 — the forbidden dimension is the name mirror's value.
    // Exactly one field differs from `admissible()`; the version and method
    // mirrors are byte-identical.
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            header_name: Some("other-weather"),
            ..admissible()
        }),
        HeaderMismatchReason::HeaderBodyNameMismatch
    );

    // Planted mutation 2 — the forbidden dimension is the version mirror's
    // value. Only `header_version` differs, and the planted value is itself a
    // real protocol version, so the refusal cannot be explained by malformation.
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some("2025-11-25"),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            ..admissible()
        }),
        HeaderMismatchReason::HeaderBodyVersionMismatch
    );

    // Planted mutation 3 — the forbidden dimension is emptiness, not absence.
    // `Some("")` is a present-but-empty header, which the mirror must refuse
    // with its own reason rather than collapsing into the missing-header case.
    assert_eq!(
        mismatch_reason(FinalHttpRequestMetadata {
            header_name: Some(""),
            ..admissible()
        }),
        HeaderMismatchReason::EmptyNameHeader
    );
    assert_ne!(
        mismatch_reason(FinalHttpRequestMetadata {
            header_name: Some(""),
            ..admissible()
        }),
        mismatch_reason(FinalHttpRequestMetadata {
            header_name: None,
            ..admissible()
        }),
        "an empty header and an absent header are different facts on the wire"
    );

    // Planted mutation 4 — `2025-11-25` mirrored on both sides is a supported-
    // version refusal and can never be a positive. The AC names this exact
    // version; asserting it here keeps that clause executable.
    let unsupported = match admit_final_request(RequestVersionMetadata {
        header_version: Some("2025-11-25"),
        body_version: Some("2025-11-25"),
    }) {
        Err(RequestAdmissionError::UnsupportedProtocolVersion(error)) => error,
        Ok(_) => panic!("2025-11-25 must never be admitted"),
        Err(other) => panic!("expected an unsupported-version refusal, got {other:?}"),
    };
    assert!(
        !unsupported
            .supported_versions()
            .contains(&"2025-11-25"),
        "the rejected version must not appear in the supported list"
    );

    // Named mutable state, byte-for-byte unchanged across every refusal above.
    let readmitted = admit_final_http_request(admissible())
        .expect("a refusal cannot poison a later admissible request");
    assert_eq!(readmitted.protocol_version().as_str(), baseline);
    assert_eq!(SUPPORTED_FINAL_PROTOCOL_VERSIONS, baseline_supported);
    assert_eq!(MCP_PROTOCOL_VERSION_HEADER, "MCP-Protocol-Version");
}

// ---------------------------------------------------------------------------
// PRT-03 B — request admission precedence and typed errors
// ---------------------------------------------------------------------------

/// A request that violates every admission rule it possibly can at once.
fn maximally_broken() -> FinalHttpRequestMetadata<'static> {
    FinalHttpRequestMetadata {
        version: RequestVersionMetadata {
            header_version: None,
            body_version: None,
        },
        header_method: None,
        body_method: None,
        header_name: None,
        body_name: None,
    }
}

/// Repairs exactly the named rule, deliberately leaving every later rule
/// violated.
///
/// Each repair installs the *minimum* value that satisfies its own rule and
/// nothing more — an empty string where emptiness is the next rule, a real but
/// unsupported version where support is the next rule, a mirrored-but-wrong
/// value where the mirror is the next rule. That is what keeps the rungs
/// contending instead of collapsing into one-rule-at-a-time tests.
///
/// The match is keyed on the *rule*, never on its ordinal, so renumbering the
/// published precedence cannot silently re-point a repair at the wrong rule.
fn repair(
    metadata: FinalHttpRequestMetadata<'static>,
    rule: FinalAdmissionRule,
) -> FinalHttpRequestMetadata<'static> {
    let FinalAdmissionRule::HeaderMismatch(reason) = rule else {
        // Repairing "unsupported version" means mirroring a supported one.
        return FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some(FINAL_PROTOCOL_VERSION),
            },
            ..metadata
        };
    };
    let version = |header, body| RequestVersionMetadata {
        header_version: header,
        body_version: body,
    };
    match reason {
        HeaderMismatchReason::MissingHeader => FinalHttpRequestMetadata {
            version: version(Some(""), metadata.version.body_version),
            ..metadata
        },
        HeaderMismatchReason::MissingBodyVersion => FinalHttpRequestMetadata {
            version: version(metadata.version.header_version, Some("")),
            ..metadata
        },
        HeaderMismatchReason::EmptyHeader => FinalHttpRequestMetadata {
            version: version(Some("2025-11-25"), metadata.version.body_version),
            ..metadata
        },
        HeaderMismatchReason::EmptyBodyVersion => FinalHttpRequestMetadata {
            version: version(metadata.version.header_version, Some("2024-11-05")),
            ..metadata
        },
        HeaderMismatchReason::HeaderBodyVersionMismatch => FinalHttpRequestMetadata {
            version: version(Some("2025-11-25"), Some("2025-11-25")),
            ..metadata
        },
        HeaderMismatchReason::MissingMethodHeader => FinalHttpRequestMetadata {
            header_method: Some(""),
            ..metadata
        },
        HeaderMismatchReason::MissingBodyMethod => FinalHttpRequestMetadata {
            body_method: Some(""),
            ..metadata
        },
        HeaderMismatchReason::EmptyMethodHeader => FinalHttpRequestMetadata {
            header_method: Some("tools/call"),
            ..metadata
        },
        HeaderMismatchReason::EmptyBodyMethod => FinalHttpRequestMetadata {
            body_method: Some("prompts/get"),
            ..metadata
        },
        HeaderMismatchReason::HeaderBodyMethodMismatch => FinalHttpRequestMetadata {
            body_method: Some("tools/call"),
            ..metadata
        },
        HeaderMismatchReason::MissingNameHeader => FinalHttpRequestMetadata {
            header_name: Some(""),
            ..metadata
        },
        HeaderMismatchReason::MissingBodyName => FinalHttpRequestMetadata {
            body_name: Some(""),
            ..metadata
        },
        HeaderMismatchReason::EmptyNameHeader => FinalHttpRequestMetadata {
            header_name: Some("weather"),
            ..metadata
        },
        HeaderMismatchReason::EmptyBodyName => FinalHttpRequestMetadata {
            body_name: Some("other-weather"),
            ..metadata
        },
        HeaderMismatchReason::HeaderBodyNameMismatch => FinalHttpRequestMetadata {
            body_name: Some("weather"),
            ..metadata
        },
    }
}

/// Does the method mirror agree on a method that requires `Mcp-Name`?
///
/// The oracle refuses to guess. It judges only the two methods these cases use,
/// and both facts are proved independently in `prt_03_a_positive`: an absent
/// name mirror is ADMITTED for `tools/list` and REFUSED with
/// `MissingNameHeader` for `tools/call`, from the same baseline.
///
/// That attribution was wrong when first written: the comment claimed both
/// facts were proved there while only the `tools/list` half was, and the
/// `tools/call` half lived in `prt_03_b_positive`'s ladder. A hard-coded
/// constant whose justification cites the wrong test is unfounded in the way
/// that matters — a reader checking it looks in the named place, does not find
/// it, and cannot tell an unproven constant from a misfiled one. Fixed by
/// supplying the missing observation rather than by softening the claim.
fn method_requires_name(metadata: &FinalHttpRequestMetadata<'_>) -> bool {
    let (Some(header), Some(body)) = (metadata.header_method, metadata.body_method) else {
        return false;
    };
    if header.is_empty() || header != body {
        return false;
    }
    match header {
        "tools/call" => true,
        "tools/list" => false,
        other => panic!("this oracle judges only the methods these cases use, not {other:?}"),
    }
}

/// An independent judgement of whether a request violates one rule.
///
/// This deliberately does **not** call the admitter. A precedence test whose
/// notion of "violated" comes from the thing under test can only ever confirm
/// itself: it would agree that the winner is the winner while proving nothing
/// about whether any other rule also applied. Deciding applicability from the
/// input, separately, is what makes the contest real.
fn violates(rule: FinalAdmissionRule, metadata: &FinalHttpRequestMetadata<'_>) -> bool {
    let header_version = metadata.version.header_version;
    let body_version = metadata.version.body_version;
    let FinalAdmissionRule::HeaderMismatch(reason) = rule else {
        // The shipped check is `header_version != FINAL_PROTOCOL_VERSION`, on
        // the header alone. Modelling it as "only once the mirror agrees" would
        // bake the *ordering* into the rule's own condition and quietly make
        // the mismatch/unsupported pair non-contending — the precedence would
        // then be untestable by construction.
        return header_version.is_some_and(|version| version != FINAL_PROTOCOL_VERSION);
    };
    // The name stage is genuinely gated: the admitter cannot know whether a
    // name is required until the method mirror has agreed on a method. That is
    // a structural dependency, not an ordering choice, and it is asserted as
    // such at the one pair where it applies.
    let name_stage_live = matches!(
        (metadata.header_method, metadata.body_method),
        (Some(header), Some(body)) if !header.is_empty() && header == body
    ) && method_requires_name(metadata);
    let differ = |header: Option<&str>, body: Option<&str>| {
        matches!((header, body), (Some(header), Some(body)) if header != body)
    };
    match reason {
        HeaderMismatchReason::MissingHeader => header_version.is_none(),
        HeaderMismatchReason::MissingBodyVersion => body_version.is_none(),
        HeaderMismatchReason::EmptyHeader => header_version == Some(""),
        HeaderMismatchReason::EmptyBodyVersion => body_version == Some(""),
        HeaderMismatchReason::HeaderBodyVersionMismatch => differ(header_version, body_version),
        HeaderMismatchReason::MissingMethodHeader => metadata.header_method.is_none(),
        HeaderMismatchReason::MissingBodyMethod => metadata.body_method.is_none(),
        HeaderMismatchReason::EmptyMethodHeader => metadata.header_method == Some(""),
        HeaderMismatchReason::EmptyBodyMethod => metadata.body_method == Some(""),
        HeaderMismatchReason::HeaderBodyMethodMismatch => {
            differ(metadata.header_method, metadata.body_method)
        }
        HeaderMismatchReason::MissingNameHeader => name_stage_live && metadata.header_name.is_none(),
        HeaderMismatchReason::MissingBodyName => name_stage_live && metadata.body_name.is_none(),
        HeaderMismatchReason::EmptyNameHeader => name_stage_live && metadata.header_name == Some(""),
        HeaderMismatchReason::EmptyBodyName => name_stage_live && metadata.body_name == Some(""),
        HeaderMismatchReason::HeaderBodyNameMismatch => {
            name_stage_live && differ(metadata.header_name, metadata.body_name)
        }
    }
}

/// The rule a given precedence order would select for this request.
fn winner_under(
    order: &[FinalAdmissionRule],
    metadata: &FinalHttpRequestMetadata<'_>,
) -> Option<FinalAdmissionRule> {
    order
        .iter()
        .copied()
        .find(|rule| violates(*rule, metadata))
}

/// The published order with two adjacent entries exchanged.
fn with_swapped(index: usize) -> Vec<FinalAdmissionRule> {
    let mut order = FINAL_ADMISSION_PRECEDENCE.to_vec();
    order.swap(index, index + 1);
    order
}

/// The request reached after repairing the first `steps` declared rules in
/// order. It violates rule `steps` and every rule after it.
fn ladder_at(steps: usize) -> FinalHttpRequestMetadata<'static> {
    let mut metadata = maximally_broken();
    for rule in &FINAL_ADMISSION_PRECEDENCE[..steps] {
        metadata = repair(metadata, *rule);
    }
    metadata
}

/// Returns the declared rule that refused this request.
fn refusing_rule(metadata: FinalHttpRequestMetadata<'_>) -> FinalAdmissionRule {
    admit_final_http_request(metadata)
        .map(|_| panic!("this request must not be admitted"))
        .unwrap_err()
        .rule()
}

#[test]
fn prt_03_b_positive() {
    // The published precedence is a contract, so its shape is asserted before
    // it is used: a duplicated rule would make the ladder below pass while
    // actually testing one rung twice.
    assert_eq!(FINAL_ADMISSION_PRECEDENCE.len(), 16);
    for (index, rule) in FINAL_ADMISSION_PRECEDENCE.iter().enumerate() {
        assert!(
            !FINAL_ADMISSION_PRECEDENCE[..index].contains(rule),
            "rule {rule:?} appears twice in the published precedence"
        );
    }

    // Walk the declared order, repairing exactly one rule per rung. At every
    // rung the request still violates EVERY later rule, so each assertion is a
    // genuine contest between the declared winner and all its successors — not
    // a test that passes because only one rule could ever have matched.
    let mut metadata = maximally_broken();
    for (index, declared) in FINAL_ADMISSION_PRECEDENCE.iter().enumerate() {
        let observed = refusing_rule(metadata);
        assert_eq!(
            observed,
            *declared,
            "at precedence position {index} the request violates this rule and all \
             {} later ones; the declared winner is {declared:?} but {observed:?} fired",
            FINAL_ADMISSION_PRECEDENCE.len() - index - 1
        );
        metadata = repair(metadata, *declared);
    }

    // Repairing the last rule must admit: if it did not, some rung above was
    // being satisfied by an unrelated repair rather than by precedence.
    let admission = admit_final_http_request(metadata)
        .expect("repairing every declared rule in order admits the request");
    assert_eq!(
        admission.protocol_version().as_str(),
        FINAL_PROTOCOL_VERSION
    );

    // Typed error surface: the required-capabilities object is carried exactly,
    // not flattened into diagnostic paths.
    let missing_capability =
        MissingRequiredClientCapabilityError::new(json!({"roots": {"listChanged": true}}))
            .expect("a bounded required-capabilities object is typed peer data");
    assert_eq!(missing_capability.http_status(), 400);
    assert_eq!(
        missing_capability.jsonrpc_error_code(),
        MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE
    );
    assert_eq!(
        missing_capability.canonical_error_data(),
        json!({"requiredCapabilities": {"roots": {"listChanged": true}}})
    );
    // The three final codes are distinct; collapsing any two would make a peer
    // unable to tell a version problem from a capability problem.
    assert_eq!(MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE, -32021);
    assert_eq!(HEADER_MISMATCH_ERROR_CODE, -32020);
    assert_eq!(UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE, -32022);
}

#[test]
fn prt_03_b_planted_negative() {
    // Named mutable state, captured before any planted input is offered.
    let published: Vec<FinalAdmissionRule> = FINAL_ADMISSION_PRECEDENCE.to_vec();
    let baseline_admission = admit_final_http_request(admissible())
        .expect("baseline admissible request")
        .protocol_version()
        .as_str()
        .to_owned();

    // Planted mutation 1 — the forbidden dimension is precedence ORDER itself,
    // for EVERY adjacent pair in the published order.
    //
    // For each pair (i, i+1): take a request that violates both, ask what the
    // shipped order predicts, then ask what the SAME order with those two
    // entries exchanged predicts. The swap must change the prediction — if it
    // does not, the pair never contended and the rung proves nothing about
    // ordering. Then require the implementation to follow the shipped answer
    // and not the swapped one. Reorder the rules and the answer changes; that
    // is what makes this a precedence test rather than a refusal test.
    let mut gated_pairs = 0;
    for index in 0..FINAL_ADMISSION_PRECEDENCE.len() - 1 {
        let contended = ladder_at(index);
        let shipped_order_predicts = winner_under(FINAL_ADMISSION_PRECEDENCE, &contended)
            .expect("the ladder request violates at least one rule");
        let swapped_order_predicts = winner_under(&with_swapped(index), &contended)
            .expect("the same request still violates at least one rule under the swap");

        if shipped_order_predicts == swapped_order_predicts {
            // The only legitimate reason a swap changes nothing is that the
            // later rule is structurally gated on the earlier one passing: the
            // admitter cannot judge the name mirror until the method mirror has
            // agreed on a method. Assert that gating explicitly instead of
            // letting a non-contending pair slip through as "fine", and require
            // the successor to become live the moment the gate is repaired.
            gated_pairs += 1;
            assert!(
                !violates(FINAL_ADMISSION_PRECEDENCE[index + 1], &contended),
                "pair ({index}, {}) did not contend, and not because the successor was \
                 inapplicable — the precedence is untested here",
                index + 1
            );
            assert!(
                violates(
                    FINAL_ADMISSION_PRECEDENCE[index + 1],
                    &repair(contended, FINAL_ADMISSION_PRECEDENCE[index])
                ),
                "the gated successor must become applicable as soon as its gate is repaired"
            );
            assert_eq!(refusing_rule(contended), FINAL_ADMISSION_PRECEDENCE[index]);
            continue;
        }

        assert_eq!(
            shipped_order_predicts,
            FINAL_ADMISSION_PRECEDENCE[index],
            "the independent oracle must agree the shipped order selects rule {index}"
        );
        assert_eq!(
            swapped_order_predicts,
            FINAL_ADMISSION_PRECEDENCE[index + 1],
            "and that the swap hands the decision to its successor"
        );

        let observed = refusing_rule(contended);
        assert_eq!(
            observed, shipped_order_predicts,
            "the implementation must follow the published order at pair ({index}, {})",
            index + 1
        );
        assert_ne!(
            observed, swapped_order_predicts,
            "the implementation must NOT behave as though the pair were exchanged"
        );
    }
    // Exactly one stage boundary is structurally gated (method -> name). If a
    // change makes more pairs non-contending, the swap-proof has quietly lost
    // coverage and this fails rather than passing with fewer real contests.
    assert_eq!(
        gated_pairs, 1,
        "exactly one adjacent pair may be structurally gated rather than ordered"
    );

    // Planted mutation 2 — the forbidden dimension is which STAGE contends. A
    // request that breaks both the version mirror and the name mirror must be
    // refused by the version stage; the name failure is real but later.
    assert_eq!(
        refusing_rule(FinalHttpRequestMetadata {
            version: RequestVersionMetadata {
                header_version: Some(FINAL_PROTOCOL_VERSION),
                body_version: Some("2024-11-05"),
            },
            header_name: Some("other-weather"),
            ..admissible()
        }),
        FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyVersionMismatch),
        "the version stage is decided before the name stage is reached"
    );
    // Repairing only the version mirror, leaving the name mirror broken,
    // surfaces the later rule — proving the name failure was present all along
    // and was being suppressed by precedence rather than by absence.
    assert_eq!(
        refusing_rule(FinalHttpRequestMetadata {
            header_name: Some("other-weather"),
            ..admissible()
        }),
        FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyNameMismatch)
    );

    // Planted mutation 3 — the forbidden dimension is the typed error's data
    // shape. A non-object required-capabilities value is refused rather than
    // coerced, so a server cannot ship a malformed capability demand.
    let error = MissingRequiredClientCapabilityError::new(json!(["roots"]))
        .expect_err("required capabilities must be the exact object, not a list of paths");
    assert_eq!(error, RequiredCapabilitiesError::NotAnObject);

    // Named mutable state, byte-for-byte unchanged across every refusal above.
    assert_eq!(FINAL_ADMISSION_PRECEDENCE, published.as_slice());
    assert_eq!(
        admit_final_http_request(admissible())
            .expect("a refusal cannot poison a later admissible request")
            .protocol_version()
            .as_str(),
        baseline_admission
    );
}
