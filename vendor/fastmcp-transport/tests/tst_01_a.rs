//! TST-01 A: canonical positive wire fixtures.
//!
//! An **external** consumer of the packaged `fastmcp-transport` crate: it
//! reaches the public codec the way a downstream crate does, via
//! `fastmcp_transport::{Codec, CodecError, InvalidMessageKind}`, and never
//! through `use super::` or a `pub(crate)` path. Nothing here is compiled
//! under `cfg(test)` inside any library (PL-3).
//!
//! # What a canonical fixture has to be
//!
//! A fixture that does not represent the wire is worse than no fixture: it
//! reports green while observing nothing, which is how several defects in this
//! codebase stayed hidden. So every fixture here is a **byte string**, not a
//! constructed value, and every positive proves three separate things:
//!
//! 1. the bytes decode through the shipped public codec,
//! 2. the decoded value carries the fields the fixture claims, and
//! 3. re-encoding reproduces the canonical bytes **byte-for-byte**.
//!
//! Point 3 is the one that matters most. "Decoded without error" is satisfied
//! by a decoder that silently drops a member; byte-identical round-trip is not.
//!
//! The framing path is exercised twice over the same corpus: once as a single
//! buffer and once **one byte at a time**, so the incremental buffering branch
//! of `Codec::decode` is genuinely reached rather than assumed. A harness that
//! only ever hands the codec whole frames never executes that branch and would
//! pass while proving nothing about it.
//!
//! # Boundary with TST-01 B
//!
//! B owns planted-invalid fixtures and the mutation oracle in its own target.
//! Per RubyOriole's ruling there is no shared module and no cross-target
//! import: B declares its own canonical starting points inline. The canonical
//! shapes this file uses are recorded in [`CANONICAL_MODERN`] and
//! [`CANONICAL_LEGACY`] so the TST-01 integration leaf can diff the two
//! corpora and surface any divergence as a finding.
//!
//! # No-claim boundary
//!
//! This leaf owns the canonical positive corpus only. It establishes no parent
//! completion, no aggregate MCP 2026-07-28 support, no MCP 2024-11-05
//! preservation, no profile maturity, conformance, publication, or release
//! readiness. Whole-message validation against the official generated schema
//! is explicitly *not* claimed — the package contract records those as known
//! incompatible and keeps them as separately failing drift fixtures.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use fastmcp_protocol::{
    ClientIngressFailureScope, JsonRpcEndpointRole, JsonRpcMessage, JsonRpcMessageDirection,
    RawJsonRpcDisposition, RequestId, dispose_raw_jsonrpc_failure,
};
use fastmcp_transport::{Codec, CodecError, InvalidMessageKind};
use serde_json::Value;

// ---------------------------------------------------------------------------
// The canonical corpus
// ---------------------------------------------------------------------------

/// One canonical wire fixture.
///
/// `canonical` is the exact frame as it appears on the wire, with no trailing
/// delimiter. Serialization order matters: these are the bytes a conforming
/// encoder must reproduce, so they are written in the order the shipped
/// serializer emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WireFixture {
    /// Stable fixture identifier, recorded in the manifest.
    id: &'static str,
    /// Whether this fixture carries every optional member or the minimum.
    populated: Population,
    /// The exact canonical frame.
    canonical: &'static str,
}

/// Whether a fixture is the minimal or the fully populated form of its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Population {
    /// Only the members the type requires.
    Minimal,
    /// Every optional member populated.
    Full,
}

impl Population {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Minimal => "minimal",
            Self::Full => "full",
        }
    }
}

/// Modern (`2026-07-28`) canonical fixtures: one minimal and one fully
/// populated per final envelope type.
const CANONICAL_MODERN: [WireFixture; 8] = [
    WireFixture {
        id: "modern/request/minimal",
        populated: Population::Minimal,
        canonical: r#"{"jsonrpc":"2.0","method":"ping","id":1}"#,
    },
    WireFixture {
        id: "modern/request/full",
        populated: Population::Full,
        canonical: r#"{"jsonrpc":"2.0","method":"tools/call","params":{"arguments":{"value":7},"name":"echo"},"id":"req-1"}"#,
    },
    WireFixture {
        id: "modern/notification/minimal",
        populated: Population::Minimal,
        canonical: r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    },
    WireFixture {
        id: "modern/notification/full",
        populated: Population::Full,
        canonical: r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1,"progressToken":"tok","total":4}}"#,
    },
    WireFixture {
        id: "modern/response-result/minimal",
        populated: Population::Minimal,
        canonical: r#"{"jsonrpc":"2.0","result":null,"id":1}"#,
    },
    WireFixture {
        id: "modern/response-result/full",
        populated: Population::Full,
        canonical: r#"{"jsonrpc":"2.0","result":{"content":[{"text":"ok","type":"text"}],"isError":false,"resultType":"tool"},"id":"req-1"}"#,
    },
    WireFixture {
        id: "modern/response-error/minimal",
        populated: Population::Minimal,
        canonical: r#"{"jsonrpc":"2.0","error":{"code":-32601,"message":"Method not found"},"id":1}"#,
    },
    WireFixture {
        id: "modern/response-error/full",
        populated: Population::Full,
        // NOTE: the error object is `code, message, data` — the DECLARATION
        // order of the `JsonRpcError` struct, which carries a plain
        // `#[derive(Serialize)]` (fastmcp-protocol/src/jsonrpc.rs:1012-1021).
        // Do not "tidy" this to alphabetical. Free-form objects elsewhere in
        // this corpus (`params`, `result` contents) ARE alphabetical, because
        // they are `serde_json::Value` maps and `preserve_order` is not
        // enabled, so `Map` is a `BTreeMap`. Two deterministic orderings
        // coexist in one frame: typed structs use declaration order, untyped
        // Value maps sort. Assuming one uniform rule is what made this fixture
        // wrong on its first execution.
        canonical: r#"{"jsonrpc":"2.0","error":{"code":-32602,"message":"Invalid params","data":{"field":"name"}},"id":"req-1"}"#,
    },
];

/// Legacy (`2024-11-05`) canonical fixtures, kept deliberately separate.
///
/// The package contract requires modern and legacy fixtures to be added
/// separately rather than sharing a corpus, because a shared corpus lets a
/// modern-only member leak into a legacy expectation unnoticed.
const CANONICAL_LEGACY: [WireFixture; 2] = [
    WireFixture {
        id: "legacy/request/minimal",
        populated: Population::Minimal,
        canonical: r#"{"jsonrpc":"2.0","method":"initialize","id":1}"#,
    },
    WireFixture {
        id: "legacy/response-result/full",
        populated: Population::Full,
        canonical: r#"{"jsonrpc":"2.0","result":{"content":[{"text":"ok","type":"text"}],"isError":false},"id":1}"#,
    },
];

/// Every fixture, modern first, in frozen order.
fn corpus() -> Vec<WireFixture> {
    let mut all = CANONICAL_MODERN.to_vec();
    all.extend_from_slice(&CANONICAL_LEGACY);
    all
}

// ---------------------------------------------------------------------------
// Round-trip through the public codec
// ---------------------------------------------------------------------------

/// Decodes a fixture and re-encodes it, returning the reproduced frame.
///
/// Re-encoding goes back through the same public codec, so the comparison is
/// against what the shipped encoder actually emits rather than against a
/// locally reconstructed string.
///
/// `#[track_caller]` so an encode failure names the fixture being round-tripped
/// instead of this helper.
#[track_caller]
fn round_trip(codec: &Codec, message: &JsonRpcMessage) -> Vec<u8> {
    match message {
        JsonRpcMessage::Request(request) => codec
            .encode_request(request)
            .expect("a canonical request re-encodes"),
        JsonRpcMessage::Response(response) => codec
            .encode_response(response)
            .expect("a canonical response re-encodes"),
    }
}

/// The corpus concatenated as a newline-delimited stream.
fn ndjson_stream(fixtures: &[WireFixture]) -> Vec<u8> {
    let mut stream = Vec::new();
    for fixture in fixtures {
        stream.extend_from_slice(fixture.canonical.as_bytes());
        stream.push(b'\n');
    }
    stream
}

// ---------------------------------------------------------------------------
// Provenance
// ---------------------------------------------------------------------------

/// Workspace root, three levels above this crate's manifest.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("workspace root exists above crates/fastmcp-transport")
        .to_path_buf()
}

/// Lowercase hex.
fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

/// SHA-256 over bytes, through the shipped bounded hash.
fn digest(bytes: &[u8]) -> [u8; 32] {
    fastmcp_core::sha256_bounded(bytes, 4 * 1024 * 1024)
        .expect("fixture material stays inside the hash bound")
        .into_bytes()
}

// ---------------------------------------------------------------------------
// tst_01_a_positive
// ---------------------------------------------------------------------------

#[test]
fn tst_01_a_positive() {
    let codec = Codec::new();
    let fixtures = corpus();
    let mut manifest = Vec::new();

    // --- Per-fixture: decode, assert, byte-identical re-encode ---------------
    for fixture in &fixtures {
        let bytes = fixture.canonical.as_bytes();

        let message = codec
            .decode_complete_message(bytes)
            .unwrap_or_else(|error| panic!("{} must decode: {error}", fixture.id));

        message.validate().unwrap_or_else(|reason| {
            panic!("{} must satisfy its invariants: {reason}", fixture.id)
        });

        // Round-trip equality. The encoder appends the frame delimiter, so the
        // expectation is the canonical frame plus exactly one newline.
        let mut expected = fixture.canonical.as_bytes().to_vec();
        expected.push(b'\n');
        let reproduced = round_trip(&codec, &message);
        assert_eq!(
            String::from_utf8_lossy(&reproduced),
            String::from_utf8_lossy(&expected),
            "{} must re-encode byte-for-byte; a fixture that merely decodes \
             proves nothing about what the encoder emits",
            fixture.id
        );

        // The decoded value must carry what the fixture's name claims it does.
        let canonical_value: Value = serde_json::from_str(fixture.canonical)
            .unwrap_or_else(|error| panic!("{} is valid JSON: {error}", fixture.id));
        match &message {
            JsonRpcMessage::Request(request) => {
                assert!(
                    fixture.id.contains("request") || fixture.id.contains("notification"),
                    "{} decoded as a request-like envelope",
                    fixture.id
                );
                assert_eq!(
                    Some(request.method.as_str()),
                    canonical_value.get("method").and_then(Value::as_str),
                    "{} must preserve its method",
                    fixture.id
                );
                // A notification is exactly a request-like envelope with no id.
                // Conflating the two is the defect the contract names when it
                // forbids adding a request ID to a core notification.
                if fixture.id.contains("notification") {
                    assert!(
                        request.id.is_none(),
                        "{} is a notification and must carry no id",
                        fixture.id
                    );
                    assert!(
                        canonical_value.get("id").is_none(),
                        "{} must omit id on the wire entirely, not spell it null",
                        fixture.id
                    );
                } else {
                    assert!(
                        request.id.is_some(),
                        "{} is a request and must carry an id",
                        fixture.id
                    );
                }
                if fixture.populated == Population::Full {
                    assert!(
                        request.params.is_some(),
                        "{} is the populated form and must carry params",
                        fixture.id
                    );
                }
            }
            JsonRpcMessage::Response(response) => {
                assert!(
                    fixture.id.contains("response"),
                    "{} decoded as a response envelope",
                    fixture.id
                );
                // Exactly one of result/error, which is the JSON-RPC invariant
                // that makes a response well-formed.
                assert_ne!(
                    response.result.is_some(),
                    response.error.is_some(),
                    "{} must carry exactly one of result or error",
                    fixture.id
                );
                if fixture.id.contains("response-error") {
                    let error = response
                        .error
                        .as_ref()
                        .unwrap_or_else(|| panic!("{} carries an error", fixture.id));
                    assert!(
                        !error.message.is_empty(),
                        "{} must carry a non-empty error message",
                        fixture.id
                    );
                    if fixture.populated == Population::Full {
                        assert!(
                            error.data.is_some(),
                            "{} is the populated form and must carry error data",
                            fixture.id
                        );
                    }
                }
                assert!(
                    response.id.is_some(),
                    "{} must correlate to a request id",
                    fixture.id
                );
            }
        }

        manifest.push(format!(
            "{}\t{}\t{}",
            fixture.id,
            fixture.populated.as_str(),
            hex(&digest(fixture.canonical.as_bytes()))
        ));
    }

    // --- Coverage: one minimal and one full per modern envelope type ---------
    for kind in [
        "modern/request",
        "modern/notification",
        "modern/response-result",
        "modern/response-error",
    ] {
        assert!(
            CANONICAL_MODERN
                .iter()
                .any(|f| f.id.starts_with(kind) && f.populated == Population::Minimal),
            "{kind} needs a minimal fixture"
        );
        assert!(
            CANONICAL_MODERN
                .iter()
                .any(|f| f.id.starts_with(kind) && f.populated == Population::Full),
            "{kind} needs a fully populated fixture"
        );
    }

    // --- Framing, exercised twice over the same corpus ----------------------
    //
    // Pass one hands the codec the whole stream. Pass two hands it one byte at
    // a time, which is the only way the incremental buffering branch of
    // `Codec::decode` is reached. A harness that only ever supplies whole
    // frames never executes that branch and proves nothing about it.
    let stream = ndjson_stream(&fixtures);

    let mut whole = Codec::new();
    let whole_messages = whole
        .decode(&stream)
        .expect("the canonical stream decodes in one pass");
    assert_eq!(
        whole_messages.len(),
        fixtures.len(),
        "every canonical frame must yield exactly one message"
    );

    let mut byte_at_a_time = Codec::new();
    let mut incremental = Vec::new();
    for byte in &stream {
        incremental.extend(
            byte_at_a_time
                .decode(std::slice::from_ref(byte))
                .expect("the canonical stream decodes incrementally"),
        );
    }
    assert_eq!(
        incremental.len(),
        fixtures.len(),
        "byte-at-a-time framing must yield the same message count as one pass"
    );

    // Both passes must agree frame for frame, re-encoded.
    for (index, (single, split)) in whole_messages.iter().zip(incremental.iter()).enumerate() {
        assert_eq!(
            round_trip(&codec, single),
            round_trip(&codec, split),
            "frame {index} ({}) must decode identically however the stream was split",
            fixtures[index].id
        );
    }

    // --- Provenance ----------------------------------------------------------
    //
    // The official generated schemas are recorded by identity, not validated
    // whole-message: the package contract keeps whole-message validation as
    // separately failing drift fixtures, so asserting it here would be a claim
    // this slice does not own.
    let legacy_schema =
        workspace_root().join("crates/fastmcp-protocol/schema/mcp-schema-2024-11-05-48234828.json");
    let legacy_bytes = std::fs::read(&legacy_schema)
        .unwrap_or_else(|error| panic!("the shipped legacy schema is readable: {error}"));
    let legacy_parsed: Value =
        serde_json::from_slice(&legacy_bytes).expect("the shipped legacy schema is valid JSON");
    assert!(
        legacy_parsed.is_object(),
        "the shipped legacy schema is a JSON object"
    );
    manifest.push(format!(
        "provenance/legacy-official-schema\t{}",
        hex(&digest(&legacy_bytes))
    ));

    // --- Manifest ------------------------------------------------------------
    manifest.sort();
    let canonical_manifest = manifest.join("\n");
    let manifest_digest = digest(canonical_manifest.as_bytes());
    // Not asserted here: re-digesting the same bytes is trivially equal and
    // would prove nothing. What the digest binds is proven in the negative,
    // where every planted mutation must move it.
    assert_ne!(
        manifest_digest, [0_u8; 32],
        "the manifest digest is not a degenerate value"
    );
    assert_eq!(
        manifest.len(),
        fixtures.len() + 1,
        "one manifest row per fixture plus one provenance row"
    );
    println!("tst_01_a_manifest_digest={}", hex(&manifest_digest));
}

// ---------------------------------------------------------------------------
// tst_01_a_planted_negative
// ---------------------------------------------------------------------------

/// What a planted mutation must reach at the codec boundary.
#[derive(Debug, Clone, Copy)]
enum Boundary {
    /// Request-like input: the codec may surface a readable id so the endpoint
    /// can answer Invalid Request.
    RequestLike,
    /// Response-like input: the codec must withhold the id, because answering
    /// a malformed response with another response is the loop the contract
    /// forbids.
    ResponseLike,
}

#[test]
fn tst_01_a_planted_negative() {
    let codec = Codec::new();

    // --- Control: the canonical form is accepted before any mutation --------
    //
    // A refusal proves something only if the unmutated fixture was accepted by
    // the same codec in the same configuration.
    let canonical_request = CANONICAL_MODERN[0].canonical;
    let canonical_response = CANONICAL_MODERN[4].canonical;
    let accepted_request = codec
        .decode_complete_message(canonical_request.as_bytes())
        .expect("the control request must be accepted before any mutation");
    let accepted_response = codec
        .decode_complete_message(canonical_response.as_bytes())
        .expect("the control response must be accepted before any mutation");
    let control_request_bytes = round_trip(&codec, &accepted_request);
    let control_response_bytes = round_trip(&codec, &accepted_response);

    // --- One wire dimension changed per case --------------------------------
    //
    // Every mutation is a one-member edit of a canonical frame above; the rest
    // of the frame is byte-identical to the accepted control.
    // `Boundary` here is the codec's InvalidMessageKind, which is a
    // CODEC-LOCAL hint about which typed parse to attempt and whether an id is
    // safe to surface. It is NOT the guard against answering server output
    // with a JSON-RPC error — that rule is enforced structurally by
    // `dispose_raw_jsonrpc_failure`, asserted separately below. An earlier
    // revision of this test asserted the loop guard here and was wrong about
    // the layer; do not move it back.
    let cases: [(&str, &str, Boundary); 7] = [
        (
            "request id spelled as an explicit null rather than omitted",
            r#"{"jsonrpc":"2.0","method":"ping","id":null}"#,
            Boundary::RequestLike,
        ),
        (
            "request carrying a duplicate id member",
            r#"{"jsonrpc":"2.0","method":"ping","id":1,"id":2}"#,
            Boundary::RequestLike,
        ),
        (
            "request declaring a non-2.0 protocol version",
            r#"{"jsonrpc":"1.0","method":"ping","id":1}"#,
            Boundary::RequestLike,
        ),
        (
            "request carrying an unknown top-level member",
            r#"{"jsonrpc":"2.0","method":"ping","id":1,"extra":true}"#,
            Boundary::RequestLike,
        ),
        (
            // Neither `method` nor `result`/`error`. On a COMPLETE scan the
            // codec has positive evidence this is not a response — a response,
            // however mangled, carries one of `result`/`error`, since that is
            // the only thing that makes it one. So it attempts the request
            // parse, which yields the accurate "missing field `method`"
            // diagnostic. The genuinely ambiguous case is a scan that aborts
            // before later members are seen, and `Codec::partial_kind` already
            // resolves that one conservatively to Response.
            "envelope carrying neither method nor result/error",
            r#"{"jsonrpc":"2.0","id":1}"#,
            Boundary::RequestLike,
        ),
        (
            "response carrying both result and error",
            r#"{"jsonrpc":"2.0","result":null,"error":{"code":-32601,"message":"x"},"id":1}"#,
            Boundary::ResponseLike,
        ),
        (
            "response error code given as a string",
            r#"{"jsonrpc":"2.0","error":{"code":"-32601","message":"x"},"id":1}"#,
            Boundary::ResponseLike,
        ),
    ];

    for (description, mutated, boundary) in cases {
        let error = codec
            .decode_complete_message(mutated.as_bytes())
            .err()
            .unwrap_or_else(|| panic!("{description} must be refused by the codec"));

        match boundary {
            Boundary::RequestLike | Boundary::ResponseLike => {
                let CodecError::InvalidMessage { kind, .. } = &error else {
                    panic!(
                        "{description} must reach the typed InvalidMessage boundary, got {error:?}"
                    );
                };
                // Direction classification is the contract's "label every
                // malformed-message fixture by endpoint role and direction".
                if matches!(boundary, Boundary::ResponseLike) {
                    assert_eq!(
                        *kind,
                        InvalidMessageKind::Response,
                        "{description} is response-like and must be classified as such"
                    );
                    assert!(
                        error.request_id().is_none(),
                        "{description} is response-like, so the codec must withhold an id: \
                         answering malformed server output with another response is the loop \
                         the contract forbids"
                    );
                }
            }
        }

        // Named mutable state unchanged: the canonical frames still decode and
        // still re-encode to the same bytes after every refusal.
        assert_eq!(
            round_trip(
                &codec,
                &codec
                    .decode_complete_message(canonical_request.as_bytes())
                    .expect("the control request survives every refusal")
            ),
            control_request_bytes,
            "{description} must leave the canonical request byte-for-byte unchanged"
        );
        assert_eq!(
            round_trip(
                &codec,
                &codec
                    .decode_complete_message(canonical_response.as_bytes())
                    .expect("the control response survives every refusal")
            ),
            control_response_bytes,
            "{description} must leave the canonical response byte-for-byte unchanged"
        );
    }

    // --- The bound, which is a different refusal class -----------------------
    let mut bounded = Codec::new();
    bounded.set_max_message_size(16);
    let oversized = CANONICAL_MODERN[1].canonical;
    assert!(
        oversized.len() > 16,
        "the oversized fixture must actually exceed the configured bound, or \
         this case would prove nothing"
    );
    let error = bounded
        .decode_complete_message(oversized.as_bytes())
        .err()
        .unwrap_or_else(|| panic!("an oversized frame must be refused"));
    assert!(
        matches!(error, CodecError::MessageTooLarge(_)),
        "an oversized frame must reach the size boundary, not a parse error: {error:?}"
    );

    // --- A refused frame must not corrupt the stream that follows it ---------
    //
    // The framing path is where a bad refusal does the most damage: a decoder
    // that leaves its buffer dirty turns one bad frame into a broken stream.
    let mut streaming = Codec::new();
    let mut stream = Vec::new();
    stream.extend_from_slice(br#"{"jsonrpc":"2.0","method":"ping","id":null}"#);
    stream.push(b'\n');
    stream.extend_from_slice(canonical_request.as_bytes());
    stream.push(b'\n');
    let first = streaming.decode(&stream);
    assert!(
        first.is_err(),
        "a stream containing a malformed frame must surface the refusal"
    );
    // After the refusal the codec must accept a clean frame again.
    let recovered = streaming
        .decode(&[canonical_request.as_bytes(), b"\n"].concat())
        .expect("the codec must accept a clean frame after refusing a malformed one");
    assert_eq!(
        recovered.len(),
        1,
        "recovery must yield exactly the one clean frame"
    );
    assert_eq!(
        round_trip(&codec, &recovered[0]),
        control_request_bytes,
        "the recovered frame must be byte-identical to the control"
    );

    // --- The manifest digest moves for every mutated fixture -----------------
    //
    // This is what makes the positive's digest meaningful: it binds the exact
    // canonical bytes, so any change to them is visible.
    let baseline = digest(canonical_request.as_bytes());
    for (description, mutated, _) in cases {
        assert_ne!(
            digest(mutated.as_bytes()),
            baseline,
            "{description} must move the fixture digest"
        );
    }

    // --- The real loop guard: client ingress can never emit a response -------
    //
    // This is the contract sentence the corpus exists to protect: "client-
    // ingress server-output fixtures record only the bounded transport/
    // connection outcome and assert an empty outbound wire."
    //
    // It is enforced structurally by role and direction, NOT by the codec's
    // InvalidMessageKind — `dispose_raw_jsonrpc_failure` does not even take
    // one. Its own doc states it: "Client ingress never obtains a
    // response-emitting branch." Asserting it here, at the layer that owns it,
    // is what an earlier revision of this test got wrong by asserting it on
    // the codec's parse hint instead.
    for scope in [
        ClientIngressFailureScope::OwningExchange,
        ClientIngressFailureScope::SharedChannel,
    ] {
        // A readable id is supplied deliberately: if the id were the thing
        // withholding the response, passing one would produce a reply. It does
        // not, because the branch is chosen by role and direction alone.
        let disposition = dispose_raw_jsonrpc_failure(
            JsonRpcEndpointRole::ClientIngress,
            JsonRpcMessageDirection::ServerToClient,
            Some(RequestId::Number(1)),
            scope,
        );
        assert!(
            matches!(
                disposition,
                RawJsonRpcDisposition::ClientOwningFailure
                    | RawJsonRpcDisposition::ClientSharedChannelFailure
            ),
            "client ingress must never reach a response-emitting branch, even with a \
             readable id; got {disposition:?} for {scope:?}"
        );
    }

    // The near-identical positive: the same malformed input at SERVER ingress
    // does get a correlated error. Without this, the assertion above would
    // also pass for an implementation that emitted nothing anywhere.
    let server_side = dispose_raw_jsonrpc_failure(
        JsonRpcEndpointRole::ServerIngress,
        JsonRpcMessageDirection::ClientToServer,
        Some(RequestId::Number(1)),
        ClientIngressFailureScope::OwningExchange,
    );
    assert!(
        matches!(server_side, RawJsonRpcDisposition::CorrelatedError(_)),
        "server ingress with a readable id must emit a correlated Invalid Request, or the \
         client-ingress assertion above proves only that nothing ever replies: got \
         {server_side:?}"
    );

    // Direction alone is not enough either: a client-role endpoint reading
    // client-to-server traffic is not an ingress path and emits nothing.
    assert!(
        matches!(
            dispose_raw_jsonrpc_failure(
                JsonRpcEndpointRole::ClientIngress,
                JsonRpcMessageDirection::ClientToServer,
                Some(RequestId::Number(1)),
                ClientIngressFailureScope::OwningExchange,
            ),
            RawJsonRpcDisposition::NoAction
        ),
        "a non-ingress role/direction pair must emit nothing"
    );

    // --- Notifications never gain an id --------------------------------------
    //
    // The contract states it directly: never add a request ID to the standard
    // core notification. A notification that grows an id is not a malformed
    // frame — it is a *different envelope type*, and conflating the two is the
    // attribution defect the contract is guarding against.
    let notification = CANONICAL_MODERN[2].canonical;
    let decoded = codec
        .decode_complete_message(notification.as_bytes())
        .expect("the canonical notification decodes");
    let JsonRpcMessage::Request(request) = &decoded else {
        panic!("a notification decodes as a request-like envelope");
    };
    assert!(
        request.id.is_none(),
        "the canonical notification carries no id"
    );

    let with_id = r#"{"jsonrpc":"2.0","method":"notifications/initialized","id":1}"#;
    let promoted = codec
        .decode_complete_message(with_id.as_bytes())
        .expect("adding an id yields a well-formed request rather than a refusal");
    let JsonRpcMessage::Request(promoted_request) = &promoted else {
        panic!("the promoted envelope is request-like");
    };
    assert_eq!(
        promoted_request.id,
        Some(RequestId::Number(1)),
        "adding an id silently promotes a notification to a request, which is \
         exactly why a core notification fixture must never carry one"
    );
}
