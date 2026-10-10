//! PRT-03 integration: public modern protocol ingress.
//!
//! This target consumes `fastmcp_protocol` exactly as a downstream crate does.
//! It touches no private item and enables no feature, so it is auto-discovered
//! under the default feature set and what passes here is what ships.
//!
//! The frozen `prt_03_i_*` names live here and nowhere else. An in-crate
//! rehearsal of the same coverage exists under `prt_03_i_unit_*` in
//! `src/lib.rs`; it was renamed out of this namespace because a `cfg(test)`
//! pair at the crate root takes the same BARE libtest names, which would give
//! two discovered tests for one required ID.
//!
//! The integration claim is narrow on purpose: A supplies version/header
//! admission, B supplies admission precedence and the typed errors, and this
//! file proves they compose on one request — that the admitted version, the
//! method/name mirrors and the typed capability error all describe the *same*
//! ingress decision rather than three independently correct mechanisms.

use fastmcp_protocol::protocol_version::{
    FINAL_ADMISSION_PRECEDENCE, FINAL_PROTOCOL_VERSION, FinalAdmissionRule,
    FinalHttpRequestMetadata, HEADER_MISMATCH_ERROR_CODE, HeaderMismatchReason,
    MCP_METHOD_HEADER, MCP_NAME_HEADER, MCP_PROTOCOL_VERSION_HEADER,
    MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE, MissingRequiredClientCapabilityError,
    RequestAdmissionError, UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE,
    admit_final_http_request,
};
use fastmcp_protocol::{
    ClientCapabilities, FinalRequestMeta, RootsCapability, SERVER_DISCOVER,
    server_discovery::SERVER_DISCOVER_METHOD,
};

/// The capabilities a server requires for the request under test.
fn required_roots() -> ClientCapabilities {
    ClientCapabilities {
        roots: Some(RootsCapability { list_changed: true }),
        ..ClientCapabilities::default()
    }
}

#[test]
fn prt_03_i_positive() {
    // A's surface: the wire header names and the final version constant.
    assert_eq!(MCP_PROTOCOL_VERSION_HEADER, "MCP-Protocol-Version");
    assert_eq!(MCP_METHOD_HEADER, "Mcp-Method");
    assert_eq!(MCP_NAME_HEADER, "Mcp-Name");
    assert_eq!(FINAL_PROTOCOL_VERSION, "2026-07-28");
    assert_eq!(SERVER_DISCOVER, SERVER_DISCOVER_METHOD);

    // The join: request metadata built from real client capabilities supplies
    // the version mirror that admission consumes. This is the wiring — the
    // version the request *carries* and the version admission *reports* have to
    // be the same fact, not two constants that happen to agree.
    let capabilities = required_roots();
    let metadata = FinalRequestMeta::new(capabilities.clone());
    let admission = admit_final_http_request(FinalHttpRequestMetadata {
        version: metadata.version_metadata(Some(FINAL_PROTOCOL_VERSION)),
        header_method: Some(SERVER_DISCOVER),
        body_method: Some(SERVER_DISCOVER),
        header_name: None,
        body_name: None,
    })
    .expect("canonical final metadata and server discovery are admitted");
    assert_eq!(
        admission.protocol_version().as_str(),
        FINAL_PROTOCOL_VERSION
    );

    // B's surface, on the same capabilities object: the typed capability error
    // carries the exact `ClientCapabilities` the server required, not a
    // flattened list of leaf paths.
    let missing = MissingRequiredClientCapabilityError::from_client_capabilities(&capabilities)
        .expect("typed required capabilities encode as final error data");
    assert_eq!(missing.http_status(), 400);
    assert_eq!(
        missing.jsonrpc_error_code(),
        MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE
    );
    assert_eq!(
        missing.canonical_error_data(),
        serde_json::json!({"requiredCapabilities": {"roots": {"listChanged": true}}})
    );

    // The three final ingress codes stay distinct through the public surface.
    // Collapsing any two would leave a peer unable to tell a header problem
    // from a version problem from a capability problem.
    assert_eq!(HEADER_MISMATCH_ERROR_CODE, -32020);
    assert_eq!(MISSING_REQUIRED_CLIENT_CAPABILITY_ERROR_CODE, -32021);
    assert_eq!(UNSUPPORTED_PROTOCOL_VERSION_ERROR_CODE, -32022);

    // B's declared precedence is reachable through the public surface and is
    // the same order the public admitter obeys. Integration consumes it; it
    // does not restate it.
    assert_eq!(FINAL_ADMISSION_PRECEDENCE.len(), 16);
    assert_eq!(
        FINAL_ADMISSION_PRECEDENCE.first(),
        Some(&FinalAdmissionRule::HeaderMismatch(
            HeaderMismatchReason::MissingHeader
        ))
    );
    assert_eq!(
        FINAL_ADMISSION_PRECEDENCE.last(),
        Some(&FinalAdmissionRule::HeaderMismatch(
            HeaderMismatchReason::HeaderBodyNameMismatch
        ))
    );
}

#[test]
fn prt_03_i_planted_negative() {
    // Named mutable state, captured before any planted input is offered. The
    // request metadata is the thing the two halves share, so it is the thing a
    // refusal must not disturb.
    let capabilities = required_roots();
    let metadata = FinalRequestMeta::new(capabilities.clone());
    let wire_before = serde_json::to_value(&metadata).expect("metadata serializes");
    let baseline = admit_final_http_request(FinalHttpRequestMetadata {
        version: metadata.version_metadata(Some(FINAL_PROTOCOL_VERSION)),
        header_method: Some(SERVER_DISCOVER),
        body_method: Some(SERVER_DISCOVER),
        header_name: None,
        body_name: None,
    })
    .expect("baseline admissible ingress")
    .protocol_version()
    .as_str()
    .to_owned();

    // The forbidden dimension is the protocol-version header, and only that.
    // Everything else — the metadata object, the method mirror, the absent
    // name mirror — is byte-identical to the accepted baseline, so the refusal
    // is attributable to the one changed field.
    let error = admit_final_http_request(FinalHttpRequestMetadata {
        version: metadata.version_metadata(Some("2025-11-25")),
        header_method: Some(SERVER_DISCOVER),
        body_method: Some(SERVER_DISCOVER),
        header_name: None,
        body_name: None,
    })
    .expect_err("changing only the protocol header must reject the request");

    // The join under test: the refusal is classified by B's declared precedence
    // and reaches A's typed header-mismatch shape. A mirror failure outranks
    // unsupported-version classification, so `2025-11-25` here is a mismatch
    // and not an unsupported version — and it can never be a positive case.
    assert_eq!(
        error.rule(),
        FinalAdmissionRule::HeaderMismatch(HeaderMismatchReason::HeaderBodyVersionMismatch),
        "the mismatched mirror outranks unsupported-version classification"
    );
    assert_ne!(error.rule(), FinalAdmissionRule::UnsupportedProtocolVersion);
    let RequestAdmissionError::HeaderMismatch(header_error) = error else {
        panic!("a mirror failure must not be classified as unsupported version");
    };
    assert_eq!(header_error.http_status(), 400);
    assert_eq!(
        header_error.jsonrpc_error_code(),
        HEADER_MISMATCH_ERROR_CODE
    );
    assert_eq!(
        header_error.canonical_error_data(),
        None,
        "canonical header-mismatch emission must not fabricate `data`"
    );

    // Named mutable state, byte-for-byte unchanged across the refusal.
    assert_eq!(
        serde_json::to_value(&metadata).expect("metadata remains serializable"),
        wire_before
    );
    assert_eq!(
        admit_final_http_request(FinalHttpRequestMetadata {
            version: metadata.version_metadata(Some(FINAL_PROTOCOL_VERSION)),
            header_method: Some(SERVER_DISCOVER),
            body_method: Some(SERVER_DISCOVER),
            header_name: None,
            body_name: None,
        })
        .expect("a refusal cannot poison a later admissible request")
        .protocol_version()
        .as_str(),
        baseline
    );
    assert_eq!(
        MissingRequiredClientCapabilityError::from_client_capabilities(&capabilities)
            .expect("the typed capability error is unaffected by an unrelated refusal")
            .canonical_error_data(),
        serde_json::json!({"requiredCapabilities": {"roots": {"listChanged": true}}})
    );
}
