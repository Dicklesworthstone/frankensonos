//! Exact, root-level PRT-01 A harness entries.
//!
//! These tests deliberately use only the public protocol crate surface so the
//! frozen `--exact` selectors name real root harness tests rather than nested
//! unit-test paths. Every refusal below travels through the shipped
//! `admit_raw_json_document` / `admit_raw_jsonrpc_document` /
//! `decode_strict_jsonrpc_message` / `dispose_raw_jsonrpc_failure` surface;
//! none of it is re-implemented here, so a regression in the crate fails these
//! tests rather than a test-local validator.

use fastmcp_protocol::{
    ClientIngressFailureScope, CorrelationKey, JsonRpcAdmissionError, JsonRpcEndpointRole,
    JsonRpcMessage, JsonRpcMessageDirection, JsonRpcRequest, JsonRpcResponse,
    MAX_JSONRPC_STRING_ID_ENCODED_BYTES, MAX_RAW_JSON_PATH_SEGMENT_BYTES,
    RawJsonAdmissionError, RawJsonRpcDisposition,
    RawJsonTopLevel, RequestId, UncorrelatedJsonRpcErrorResponse, admit_raw_json_document,
    admit_raw_jsonrpc_document, decode_strict_jsonrpc_message, dispose_raw_jsonrpc_failure,
};
use serde_json::{Value, json};

/// The bound every case below hands the production admission entry points.
const DOCUMENT_LIMIT: usize = 4 * 1024;

/// Decodes through the shipped entry point, asserting admission.
fn admit(frame: &[u8]) -> JsonRpcMessage {
    decode_strict_jsonrpc_message(frame, DOCUMENT_LIMIT)
        .expect("the shipped raw-admission and envelope decoder admit this frame")
}

/// Decodes through the shipped entry point, asserting refusal.
fn refuse(frame: &[u8]) -> JsonRpcAdmissionError {
    decode_strict_jsonrpc_message(frame, DOCUMENT_LIMIT)
        .expect_err("the shipped decoder must refuse this frame")
}

#[test]
fn prt_01_envelopes_positive() {
    // Each production union is admitted and parsed into the right shape, not
    // merely "not rejected".
    let JsonRpcMessage::Request(request) =
        admit(br#"{"jsonrpc":"2.0","method":"tools/list","id":42}"#)
    else {
        panic!("a client-to-server request must decode as a request envelope");
    };
    assert_eq!(request.id, Some(RequestId::Number(42)));
    assert_eq!(request.method, "tools/list");
    assert_eq!(request.params, None);
    assert!(!request.is_notification());

    let JsonRpcMessage::Request(notification) =
        admit(br#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
    else {
        panic!("a notification must decode as a request envelope without an id");
    };
    assert_eq!(notification.id, None);
    assert!(notification.is_notification());

    let JsonRpcMessage::Response(success) =
        admit(br#"{"jsonrpc":"2.0","result":null,"id":"request-42"}"#)
    else {
        panic!("a success response must decode as a response envelope");
    };
    assert_eq!(success.result, Some(Value::Null));
    assert_eq!(success.error, None);
    assert_eq!(success.id, Some(RequestId::String("request-42".to_owned())));
    assert!(!success.is_error());

    let JsonRpcMessage::Response(failure) =
        admit(br#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"missing"},"id":42}"#)
    else {
        panic!("an error response must decode as a response envelope");
    };
    assert_eq!(failure.result, None);
    assert!(failure.is_error());
    assert_eq!(failure.id, Some(RequestId::Number(42)));
    let error = failure
        .error
        .expect("the admitted error member is retained");
    assert_eq!(error.code.as_i32(), Some(-32601));
    assert_eq!(error.message, "missing");

    // This repository carries two JSON member orderings in one frame: a typed
    // struct serializes in field-declaration order, while a `serde_json::Value`
    // map sorts its members alphabetically. Both are observable here, so a
    // future change to either ordering fails this positive rather than
    // silently changing the wire.
    let ordered = JsonRpcRequest::new(
        "tools/call",
        Some(json!({"zebra": 1, "alpha": 2})),
        RequestId::Number(7),
    );
    assert_eq!(
        serde_json::to_string(&ordered).expect("the typed request serializes"),
        r#"{"jsonrpc":"2.0","method":"tools/call","params":{"alpha":2,"zebra":1},"id":7}"#,
        "envelope members follow declaration order while Value params sort alphabetically",
    );
}

#[test]
fn prt_01_envelopes_planted_negative() {
    // Every case below is one admitted baseline plus one forbidden dimension.
    // The accepted ledger is rebuilt from the baseline each time and must be
    // byte-for-byte unchanged after the refusal.
    let request_baseline = br#"{"jsonrpc":"2.0","method":"tools/list","id":42}"#;
    let response_baseline = br#"{"jsonrpc":"2.0","result":null,"id":42}"#;

    let planted: [(&[u8], &[u8], JsonRpcAdmissionError); 5] = [
        // Duplicate `id` member: raw admission refuses before typed decode.
        (
            request_baseline,
            br#"{"jsonrpc":"2.0","method":"tools/list","id":42,"id":42}"#,
            JsonRpcAdmissionError::Raw(RawJsonAdmissionError::DuplicateObjectMember),
        ),
        // Only the version literal changes.
        (
            request_baseline,
            br#"{"jsonrpc":"2.1","method":"tools/list","id":42}"#,
            JsonRpcAdmissionError::InvalidEnvelope,
        ),
        // Only the outcome members change: both present.
        (
            response_baseline,
            br#"{"jsonrpc":"2.0","result":null,"error":{"code":-32601,"message":"x"},"id":42}"#,
            JsonRpcAdmissionError::InvalidEnvelope,
        ),
        // Only the outcome members change: neither present.
        (
            response_baseline,
            br#"{"jsonrpc":"2.0","id":42}"#,
            JsonRpcAdmissionError::InvalidEnvelope,
        ),
        // Only the id spelling changes: an explicit null is never an id.
        (
            response_baseline,
            br#"{"jsonrpc":"2.0","result":null,"id":null}"#,
            JsonRpcAdmissionError::InvalidEnvelope,
        ),
    ];

    for (baseline, forbidden, expected) in planted {
        let serialized_before =
            serde_json::to_string(&admit(baseline)).expect("admitted frames re-serialize");

        assert_eq!(
            refuse(forbidden),
            expected,
            "planted frame {} must be refused for exactly its forbidden dimension",
            String::from_utf8_lossy(forbidden),
        );

        // The refusal yielded no message, and the production decode path still
        // reproduces the admitted frame byte-for-byte afterwards. Registry,
        // waiter, cache, and session state is the PRT-01 integration boundary
        // (ahet.5.3), not this one.
        assert_eq!(
            serde_json::to_string(&admit(baseline)).expect("admitted frames re-serialize"),
            serialized_before,
            "the rejected frame left the admitted decode byte-for-byte unchanged",
        );
    }
}

#[test]
fn prt_01_id_correlation_positive() {
    // Canonical numeric spellings collapse to one registry key; a string with
    // the same digits stays in its own namespace.
    let integer = RequestId::Number(1);
    for alias in ["1.0", "1e0", "1E0", "0.1e1"] {
        assert_eq!(
            integer.correlation_key().expect("valid numeric ID"),
            RequestId::Integer(alias.to_owned())
                .correlation_key()
                .expect("valid mathematical integer ID"),
            "numeric spelling {alias} must share one canonical key with 1",
        );
        assert!(integer.correlates_with(&RequestId::Integer(alias.to_owned())));
    }
    assert_eq!(
        RequestId::Number(0).correlation_key().expect("zero"),
        RequestId::Integer("-0".to_owned())
            .correlation_key()
            .expect("signed zero is a mathematical integer"),
        "signed-zero spellings share the canonical zero key",
    );
    assert_ne!(
        integer.correlation_key().expect("valid numeric ID"),
        CorrelationKey::String("1".to_owned()),
        "string and numeric namespaces remain disjoint",
    );

    // Values beyond 2^53 keep their exact identity. Under f64 correlation
    // these two would collide; under canonical integer keys they do not.
    let near = RequestId::Integer("9007199254740992".to_owned());
    let next = RequestId::Integer("9007199254740993".to_owned());
    assert_ne!(
        near.correlation_key().expect("mathematical integer"),
        next.correlation_key().expect("mathematical integer"),
        "adjacent integers beyond 2^53 must not collide through f64",
    );
    assert_eq!(
        RequestId::Number(9_007_199_254_740_993)
            .correlation_key()
            .expect("valid numeric ID"),
        next.correlation_key().expect("mathematical integer"),
        "the i64 and lexeme spellings of one value share a key",
    );

    // An arbitrary-precision id is echoed back byte-for-byte, in both signs.
    for large in [
        "922337203685477580812345678901234567890",
        "-922337203685477580812345678901234567890",
    ] {
        let raw = format!(r#"{{"jsonrpc":"2.0","method":"tools/list","id":{large}}}"#);
        let JsonRpcMessage::Request(request) = admit(raw.as_bytes()) else {
            panic!("the admitted frame must remain a request");
        };
        assert_eq!(
            request.id,
            Some(RequestId::Integer(large.to_owned())),
            "the admitted lexeme is retained rather than rounded",
        );
        let echoed = serde_json::to_string(&JsonRpcResponse::success(
            request.id.expect("the request remains correlated"),
            Value::Null,
        ))
        .expect("response serializes");
        assert!(
            echoed.contains(large),
            "the response must echo {large} exactly, got {echoed}",
        );
    }

    // Unknown application error codes keep arbitrary precision at the raw
    // envelope boundary instead of being classified through f64.
    let code = "123456789012345678901234567890";
    let raw = format!(r#"{{"jsonrpc":"2.0","error":{{"code":{code},"message":"x"}},"id":1}}"#);
    let JsonRpcMessage::Response(response) = admit(raw.as_bytes()) else {
        panic!("the admitted frame must remain a response");
    };
    let error = response.error.expect("the error member is retained");
    assert_eq!(error.code.as_str(), code, "the code lexeme is exact");
    assert_eq!(
        error.code.as_i32(),
        None,
        "a code outside i32 has no legacy classification",
    );
}

#[test]
fn prt_01_id_correlation_planted_negative() {
    // Baseline: one admitted mathematical-integer key, re-derived through the
    // production accessor after every refusal below.
    let baseline = RequestId::Integer("1".to_owned());
    let admitted_key = baseline
        .correlation_key()
        .expect("the mathematical-integer baseline yields a key");

    // Only the fractional part differs from the admitted baseline.
    assert!(
        RequestId::Integer("1.5".to_owned())
            .correlation_key()
            .is_err(),
        "a directly constructed fractional Integer lexeme cannot reach a registry key",
    );
    // Only the exponent integrality differs.
    assert!(
        RequestId::Integer("1e-1".to_owned())
            .correlation_key()
            .is_err(),
        "a non-integral exponent form cannot reach a registry key",
    );
    // Only the encoded length differs from an admitted string id.
    let over_limit = "x".repeat(MAX_JSONRPC_STRING_ID_ENCODED_BYTES);
    assert!(
        RequestId::String(over_limit).correlation_key().is_err(),
        "an over-limit string id cannot reach a registry key",
    );
    // Namespaces never alias, so a string can never complete a numeric waiter.
    assert!(
        !RequestId::String("1".to_owned()).correlates_with(&RequestId::Number(1)),
        "a string id must never correlate with a numeric id",
    );
    assert_eq!(
        baseline.correlation_key().expect("the baseline still keys"),
        admitted_key,
        "the rejected ids leave the admitted correlation key unchanged",
    );

    // The same forbidden dimensions are refused on the wire, one variable at
    // a time against admitted baselines.
    let _ = admit(br#"{"jsonrpc":"2.0","method":"tools/list","id":1}"#);
    assert_eq!(
        refuse(br#"{"jsonrpc":"2.0","method":"tools/list","id":1.5}"#),
        JsonRpcAdmissionError::InvalidEnvelope,
        "a fractional wire id is refused",
    );
    let _ = admit(br#"{"jsonrpc":"2.0","error":{"code":-32600,"message":"x"},"id":1}"#);
    assert_eq!(
        refuse(br#"{"jsonrpc":"2.0","error":{"code":-32600.5,"message":"x"},"id":1}"#),
        JsonRpcAdmissionError::InvalidEnvelope,
        "a fractional error code is refused before typed error decoding",
    );
    assert_eq!(
        baseline.correlation_key().expect("the baseline still keys"),
        admitted_key,
        "wire refusals leave the admitted correlation key unchanged",
    );
}

#[test]
fn prt_01_duplicate_member_planted_negative() {
    // One admitted baseline per case; only a repeated member name differs.
    let cases: [(&[u8], &[u8], &str); 4] = [
        (
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"cursor":"a"}}"#,
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"cursor":"a","cursor":"b"}}"#,
            "/params/cursor",
        ),
        // Duplicates are refused at every nesting level, not just the envelope.
        (
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"filter":{"name":"a"}}}"#,
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"filter":{"name":"a","name":"b"}}}"#,
            "/params/filter/name",
        ),
        // Including inside arrays, where the path carries the element index.
        (
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"items":[{"k":1},{"k":1}]}}"#,
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"items":[{"k":1},{"k":1,"k":2}]}}"#,
            "/params/items/1/k",
        ),
        // A member name cannot forge a path separator or leak its own bytes:
        // everything outside `[A-Za-z0-9_.-]` is redacted to `*`.
        (
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"a b/c":1}}"#,
            br#"{"jsonrpc":"2.0","method":"tools/list","params":{"a b/c":1,"a b/c":2}}"#,
            "/params/a*b*c",
        ),
    ];

    for (baseline, planted, expected_path) in cases {
        let serialized_before =
            serde_json::to_string(&admit(baseline)).expect("admitted frames re-serialize");

        // The JSON-RPC entry point reports the stable structural reason.
        assert_eq!(
            refuse(planted),
            JsonRpcAdmissionError::Raw(RawJsonAdmissionError::DuplicateObjectMember),
            "duplicate members are refused before typed decoding",
        );
        // The reusable primitive reports the same reason plus a redacted path.
        let failure =
            admit_raw_json_document(planted, DOCUMENT_LIMIT, RawJsonTopLevel::JsonRpcObject)
                .expect_err("the reusable primitive refuses the same document");
        assert_eq!(
            failure.error(),
            RawJsonAdmissionError::DuplicateObjectMember
        );
        assert_eq!(
            failure.path(),
            expected_path,
            "the refusal names its structural location and redacts the rest",
        );

        assert_eq!(
            serde_json::to_string(&admit(baseline)).expect("admitted frames re-serialize"),
            serialized_before,
            "duplicate parameters reach no typed state",
        );
    }

    // The identical duplicate rule governs a security-bearing document, so two
    // consumers of one document cannot disagree about which member they saw.
    let jwks = br#"{"keys":[{"kty":"RSA","kty":"EC"}]}"#;
    let failure = admit_raw_json_document(
        jwks,
        DOCUMENT_LIMIT,
        RawJsonTopLevel::SecurityDocumentObject,
    )
    .expect_err("a duplicate JWK member is refused by the same bounded pass");
    assert_eq!(
        failure.error(),
        RawJsonAdmissionError::DuplicateObjectMember
    );
    assert_eq!(failure.path(), "/keys/0/kty");
}

#[test]
fn prt_01_top_level_batch_array_planted_negative() {
    let array_of_one = br#"[{"jsonrpc":"2.0","method":"tools/list"}]"#;
    let mixed_array = br#"[{"jsonrpc":"2.0","method":"tools/list"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#;

    for batch in [array_of_one.as_slice(), mixed_array.as_slice()] {
        assert_eq!(
            admit_raw_jsonrpc_document(batch, DOCUMENT_LIMIT),
            Err(RawJsonAdmissionError::TopLevelBatch),
            "the raw public admission gate rejects a batch before typed decode",
        );
        // The full decode path stops at the same boundary, so no envelope,
        // correlation key, or typed member is ever constructed from a batch.
        assert_eq!(
            refuse(batch),
            JsonRpcAdmissionError::Raw(RawJsonAdmissionError::TopLevelBatch),
            "typed decoding is never reached for a top-level array",
        );

        // Only the selected top-level policy changes. A security document has
        // no batch concept, so the same bytes are an ordinary shape violation
        // and never inherit JSON-RPC batch vocabulary.
        let failure = admit_raw_json_document(
            batch,
            DOCUMENT_LIMIT,
            RawJsonTopLevel::SecurityDocumentObject,
        )
        .expect_err("a security document must be a single object");
        assert_eq!(failure.error(), RawJsonAdmissionError::TopLevelNotObject);
        assert_eq!(failure.path(), "");

        // The refusal is a deliberate JSON-RPC policy, not a scanner
        // limitation: the identical bytes are structurally admissible when the
        // caller asks for any single top-level value.
        assert!(
            admit_raw_json_document(batch, DOCUMENT_LIMIT, RawJsonTopLevel::AnyValue).is_ok(),
            "the bounded scanner itself can walk the array it refuses to admit as JSON-RPC",
        );
    }
}

#[test]
fn prt_01_a_positive() {
    // The shipped public surface admits an ordinary request.
    let JsonRpcMessage::Request(request) =
        admit(br#"{"jsonrpc":"2.0","method":"tools/list","id":"public-request"}"#)
    else {
        panic!("the shipped decoder must admit this request");
    };
    assert_eq!(
        request.id,
        Some(RequestId::String("public-request".to_owned())),
    );

    // The same bounded pass serves a security-bearing document unchanged,
    // which is the reuse PRT-01 B builds on.
    assert!(
        admit_raw_json_document(
            br#"{"keys":[{"kty":"RSA","n":"AQAB","e":"AQAB"}]}"#,
            DOCUMENT_LIMIT,
            RawJsonTopLevel::SecurityDocumentObject,
        )
        .is_ok(),
        "a well-formed JWKS is admitted by the reusable primitive",
    );

    // Server ingress with one readable valid id echoes that exact id.
    let readable = RequestId::Integer("922337203685477580812345678901234567890".to_owned());
    let disposition = dispose_raw_jsonrpc_failure(
        JsonRpcEndpointRole::ServerIngress,
        JsonRpcMessageDirection::ClientToServer,
        Some(readable.clone()),
        ClientIngressFailureScope::OwningExchange,
    );
    let RawJsonRpcDisposition::CorrelatedError(correlated) = disposition else {
        panic!("server ingress with a readable id must produce a correlated error");
    };
    assert_eq!(correlated.id, Some(readable));
    let wire = serde_json::to_string(&correlated).expect("the correlated error serializes");
    assert!(
        wire.contains("922337203685477580812345678901234567890"),
        "the correlated error echoes the admitted lexeme exactly, got {wire}",
    );

    // Server ingress with no readable id produces the sealed uncorrelated
    // response, whose `id` member is absent rather than null.
    let disposition = dispose_raw_jsonrpc_failure(
        JsonRpcEndpointRole::ServerIngress,
        JsonRpcMessageDirection::ClientToServer,
        None,
        ClientIngressFailureScope::OwningExchange,
    );
    let RawJsonRpcDisposition::UncorrelatedError(uncorrelated) = disposition else {
        panic!("server ingress without a readable id must produce the sealed response");
    };
    assert_eq!(uncorrelated.error().code.as_i32(), Some(-32700));
    let wire = serde_json::to_string(&uncorrelated).expect("the sealed response serializes");
    assert!(
        !wire.contains("\"id\""),
        "an uncorrelated error omits id entirely rather than sending null, got {wire}",
    );
    // The sealing runs in the direction that protects the registry: no
    // id-bearing body can become an uncorrelated response, so this type can
    // never carry a correlation key into a waiter map.
    assert!(
        serde_json::from_str::<UncorrelatedJsonRpcErrorResponse>(
            r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"x"},"id":1}"#
        )
        .is_err(),
        "an id-bearing body must never deserialize into the uncorrelated response",
    );
    assert!(
        serde_json::from_str::<UncorrelatedJsonRpcErrorResponse>(
            r#"{"jsonrpc":"2.0","result":null,"id":1}"#
        )
        .is_err(),
        "a success body must never deserialize into the uncorrelated response",
    );
    assert!(
        serde_json::from_str::<UncorrelatedJsonRpcErrorResponse>(&wire).is_ok(),
        "the sealed payload round-trips as itself",
    );
}

#[test]
fn prt_01_a_planted_negative() {
    let baseline = br#"{"jsonrpc":"2.0","method":"tools/list","id":"public-request"}"#;
    let serialized_before =
        serde_json::to_string(&admit(baseline)).expect("admitted frames re-serialize");

    // A leading byte-order mark differs from the baseline only in its first
    // three bytes and is never stripped or repaired.
    let mut bom = baseline.to_vec();
    bom.splice(0..0, [0xef, 0xbb, 0xbf]);
    assert_eq!(
        refuse(&bom),
        JsonRpcAdmissionError::Raw(RawJsonAdmissionError::ByteOrderMark),
    );

    // Direct JSON is never replacement-decoded: each malformed UTF-8 sequence
    // below replaces exactly one admitted ASCII byte of the baseline's id.
    let malformed: [(&str, &[u8]); 4] = [
        ("truncated two-byte lead", &[0xc3]),
        ("overlong encoding of NUL", &[0xc0, 0x80]),
        ("UTF-16 surrogate encoded as UTF-8", &[0xed, 0xa0, 0x80]),
        ("isolated continuation byte", &[0x80]),
    ];
    for (label, sequence) in malformed {
        let prefix = br#"{"jsonrpc":"2.0","method":"tools/list","id":"pub"#;
        let suffix = br#"lic"}"#;
        let mut planted = prefix.to_vec();
        planted.extend_from_slice(sequence);
        planted.extend_from_slice(suffix);
        assert_eq!(
            refuse(&planted),
            JsonRpcAdmissionError::Raw(RawJsonAdmissionError::InvalidUtf8),
            "{label} must be refused, never replacement-decoded",
        );
        let failure =
            admit_raw_json_document(&planted, DOCUMENT_LIMIT, RawJsonTopLevel::JsonRpcObject)
                .expect_err("the reusable primitive refuses the same bytes");
        assert_eq!(failure.error(), RawJsonAdmissionError::InvalidUtf8);
        assert_eq!(
            failure.path(),
            "",
            "a whole-document byte failure reports the root path",
        );
    }

    // Only the endpoint role changes. Identical malformed input that produced
    // a permitted server-ingress response above yields no outbound wire action
    // at client ingress, for either transport ownership.
    for (scope, expected) in [
        (
            ClientIngressFailureScope::OwningExchange,
            RawJsonRpcDisposition::ClientOwningFailure,
        ),
        (
            ClientIngressFailureScope::SharedChannel,
            RawJsonRpcDisposition::ClientSharedChannelFailure,
        ),
    ] {
        let disposition = dispose_raw_jsonrpc_failure(
            JsonRpcEndpointRole::ClientIngress,
            JsonRpcMessageDirection::ServerToClient,
            Some(RequestId::Number(1)),
            scope,
        );
        assert_eq!(
            disposition, expected,
            "client ingress must not gain a response-emitting branch from a readable id",
        );
        assert!(
            !matches!(
                disposition,
                RawJsonRpcDisposition::CorrelatedError(_)
                    | RawJsonRpcDisposition::UncorrelatedError(_)
            ),
            "no client-ingress branch may emit a JSON-RPC response",
        );
    }

    // A reversed direction on either role is not an ingress path and emits
    // nothing at all.
    for role in [
        JsonRpcEndpointRole::ServerIngress,
        JsonRpcEndpointRole::ClientIngress,
    ] {
        let reversed = match role {
            JsonRpcEndpointRole::ServerIngress => JsonRpcMessageDirection::ServerToClient,
            JsonRpcEndpointRole::ClientIngress => JsonRpcMessageDirection::ClientToServer,
        };
        assert_eq!(
            dispose_raw_jsonrpc_failure(
                role,
                reversed,
                Some(RequestId::Number(1)),
                ClientIngressFailureScope::OwningExchange,
            ),
            RawJsonRpcDisposition::NoAction,
            "a reversed direction union must not escape into a wire action",
        );
    }

    // Nothing above disturbed the admitted decode path.
    let readmitted = admit(baseline);
    assert!(matches!(
        &readmitted,
        JsonRpcMessage::Request(request)
            if request.id == Some(RequestId::String("public-request".to_owned()))
    ));
    assert_eq!(
        serde_json::to_string(&readmitted).expect("admitted frames re-serialize"),
        serialized_before,
        "the planted raw rejections leave the admitted decode unchanged",
    );

    // The refusals are not an artifact of the helper: the same production
    // error type is produced for a bound violation, and a well-formed frame
    // still passes.
    let too_large = admit_raw_jsonrpc_document(baseline, baseline.len() - 1);
    assert_eq!(too_large, Err(RawJsonAdmissionError::DocumentTooLarge));
    assert!(admit_raw_jsonrpc_document(baseline, DOCUMENT_LIMIT).is_ok());
}

/// Binds the character-counter / byte-budget equivalence in the path redactor.
/// Supplementary coverage, deliberately NOT a frozen acceptance ID.
///
/// # Why prose was not enough
///
/// `redact_raw_json_path` counts CHARACTERS and compares them against
/// `MAX_RAW_JSON_PATH_SEGMENT_BYTES`, a BYTE budget. That is correct only
/// because every retained character contributes exactly one byte: the
/// retention set is ASCII alphanumerics plus `_ . -`, and every other
/// character collapses to a single `*`. A comment at the loop records that
/// reasoning, and **a comment cannot fail**.
///
/// Two readers reached that conclusion independently and agreed — but by the
/// same method, tracing the same branches, so the agreement was one reading
/// with extra confidence rather than two confirmations. This test is the
/// independent instrument: it pushes the defect through the function instead
/// of reasoning about it.
///
/// # The worthless implementation this refuses
///
/// Every other path assertion in this file feeds the redactor an ASCII member
/// name, so a redactor that passed names through unchanged would satisfy all
/// of them. Widen the retention set to admit a multi-byte character — or reach
/// for a Unicode ellipsis as the truncation marker — and the counter keeps
/// counting characters while the output grows to two, three or four bytes
/// each. The bound silently stops holding. This case fails when that happens;
/// nothing else here would.
#[test]
fn prt_01_a_redacted_path_bounds_bytes_not_characters() {
    // A name whose CHARACTER count and BYTE count differ by a factor of two
    // before redaction. If this fixture ever became ASCII the test would pass
    // while proving nothing, so it is asserted rather than assumed.
    let name: String = "é".repeat(100);
    assert_eq!(name.chars().count(), 100);
    assert_eq!(
        name.len(),
        200,
        "the fixture must be multi-byte or this case proves nothing"
    );

    let document = format!("{{\"{name}\":1,\"{name}\":2}}");
    let failure =
        admit_raw_json_document(document.as_bytes(), DOCUMENT_LIMIT, RawJsonTopLevel::AnyValue)
            .expect_err("a duplicate member is refused whatever its name encodes to");
    assert_eq!(
        failure.error(),
        RawJsonAdmissionError::DuplicateObjectMember
    );

    let path = failure.path();

    // THE BINDING ASSERTION. Pure ASCII is exactly the property that makes a
    // character count equal a byte count, so this is the invariant itself
    // rather than a restatement of it.
    assert!(
        path.is_ascii(),
        "the redacted path must be pure ASCII or the character counter stops bounding bytes: {path:?}"
    );

    // The budget, measured in BYTES, against the declared design limit rather
    // than a frozen measurement of today's output.
    for segment in path.split('/').skip(1) {
        assert!(
            segment.len() <= MAX_RAW_JSON_PATH_SEGMENT_BYTES + 1,
            "segment {segment:?} is {} bytes, past the {MAX_RAW_JSON_PATH_SEGMENT_BYTES}-byte \
             budget plus its one-byte truncation marker",
            segment.len()
        );
    }

    // Every non-retained character collapses to exactly one `*`, and the
    // segment is truncated with `~`. Computed from the public limit so that
    // raising the limit moves the expectation instead of rotting it.
    let expected = format!("/{}~", "*".repeat(MAX_RAW_JSON_PATH_SEGMENT_BYTES));
    assert_eq!(path, expected);
}
