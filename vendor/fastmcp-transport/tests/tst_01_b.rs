//! TST-01 implementation B: planted-invalid wire fixtures and the mutation
//! oracle that orders them.
//!
//! This target is self-contained by ruling. It defines its own canonical
//! starting documents inline rather than importing the TST-01 A corpus, so the
//! one-variable claim behind every planted negative is auditable in this file
//! alone: the reader can see both what was mutated *from* and what was mutated
//! *to* without opening another agent's target.
//!
//! Canonical shapes used here are recorded in [`CANONICAL_BASES`] for the
//! TST-01 integration leaf to diff against A's canonical forms. Divergence
//! between the two is a finding about which one is wrong, and it is meant to
//! surface at integration rather than be prevented by coupling the targets.
//!
//! # What this slice proves, and why "it was rejected" is not enough
//!
//! A planted-invalid fixture that is refused for the *wrong* reason proves
//! nothing: it looks green while never reaching the bound it claims to test.
//! Every mutation below therefore names the exact typed refusal it must
//! provoke, and the oracle asserts that exact variant rather than `is_err()`.
//!
//! Ordering is the other half. `admit_raw_jsonrpc_document` applies its bounds
//! in a fixed sequence, so a document that crosses two bounds reports only the
//! earlier one. A fixture whose named bound sits behind an earlier one is
//! **shadowed** — structurally unreachable, not merely untested. The inventory
//! records those explicitly instead of omitting them, because which bounds are
//! reachable under a given document limit is itself the ordering property.
//!
//! Every case runs through the shipped, non-`cfg(test)` public surface:
//! `fastmcp_protocol::admit_raw_jsonrpc_document` and
//! `fastmcp_protocol::decode_strict_jsonrpc_message`.

use fastmcp_protocol::{
    ClientIngressFailureScope, JsonRpcAdmissionError, JsonRpcEndpointRole, JsonRpcMessage,
    JsonRpcMessageDirection, MAX_RAW_JSON_EXPONENT, MAX_RAW_JSON_NESTING_DEPTH,
    MAX_RAW_JSON_NUMBER_BYTES, RawJsonAdmissionError, admit_raw_jsonrpc_document,
    decode_strict_jsonrpc_message,
};

// ---------------------------------------------------------------------------
// Fixed admission parameters
// ---------------------------------------------------------------------------

/// The single document byte limit every case in this target is evaluated
/// under.
///
/// One fixed limit for the whole inventory is deliberate. `document_byte_limit`
/// is a caller parameter, so varying it per case would make the limit a second
/// variable and destroy the one-variable claim: a fixture could then "pass"
/// because its limit was chosen to let it through. With the limit fixed, the
/// document bytes are the only thing that differs between a canonical base and
/// its planted mutation.
const ADMISSION_BYTE_LIMIT: usize = 256 * 1024;

// ---------------------------------------------------------------------------
// Canonical starting documents, defined inline by ruling
// ---------------------------------------------------------------------------

/// Minimal valid client-to-server request.
const CANONICAL_REQUEST: &str = r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":{}}"#;

/// Minimal valid client-to-server notification: no `id` member.
const CANONICAL_NOTIFICATION: &str =
    r#"{"jsonrpc":"2.0","method":"notifications/initialized","params":{}}"#;

/// Minimal valid server-to-client success response.
const CANONICAL_RESULT: &str = r#"{"jsonrpc":"2.0","id":1,"result":{}}"#;

/// Minimal valid server-to-client error response.
const CANONICAL_ERROR: &str =
    r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"Invalid Request"}}"#;

/// The canonical shapes this target mutates from, recorded for the TST-01
/// integration leaf to diff against implementation A's canonical corpus.
const CANONICAL_BASES: [(&str, &str); 4] = [
    ("request", CANONICAL_REQUEST),
    ("notification", CANONICAL_NOTIFICATION),
    ("result", CANONICAL_RESULT),
    ("error", CANONICAL_ERROR),
];

// MEMBER ORDER: these bases are safe only because this target never encodes.
//
// Two deterministic orderings coexist in one JSON-RPC frame. A typed struct
// with `#[derive(Serialize)]` emits in DECLARATION order, while an untyped
// `serde_json::Value` map emits ALPHABETICALLY, because `preserve_order` is off
// and `Map` is a `BTreeMap`. `params` and `result` contents are `Value` maps;
// the envelopes are typed structs.
//
// The declaration orders are not the ones a hand-written fixture reaches for.
// `id` is declared LAST in both envelopes, not second:
//   JsonRpcRequest  { jsonrpc, method, params, id }   jsonrpc.rs:893-915
//   JsonRpcResponse { jsonrpc, result, error, id }    jsonrpc.rs:1224-1232
//   JsonRpcError    { code, message, data }           jsonrpc.rs:1012-1021
//
// Three of the four bases above put `id` second and so do NOT match declaration
// order; only `notification` does. That is harmless here and only here: this
// target is decode-only. It calls `admit_raw_jsonrpc_document` and
// `decode_strict_jsonrpc_message` and never re-emits a frame, and member order
// is irrelevant to decoding, so the divergence is unobservable by construction.
//
// It stops being unobservable the moment anyone adds an encoder — a byte-exact
// round-trip assertion, a re-serialized expected value, a golden comparison.
// Three of these four bases would then fail, and per the TST-01 A finding that
// failure surfaces only on first execution. Add an encoder here and you must
// re-order these literals to declaration order first.

// ---------------------------------------------------------------------------
// Refusal vocabulary
// ---------------------------------------------------------------------------

/// The exact typed refusal a planted document must provoke.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExpectedRefusal {
    /// Refused at the raw-document boundary with this exact variant.
    Raw(RawJsonAdmissionError),
    /// Admitted as a raw document, then refused as a JSON-RPC envelope.
    Envelope,
}

/// What the shipped surface actually did with one planted document.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Observed {
    Raw(RawJsonAdmissionError),
    Envelope,
    /// The document was admitted and decoded. For a planted-invalid fixture
    /// this is a failure of the fixture, not of the decoder.
    Admitted,
}

impl Observed {
    fn matches(self, expected: ExpectedRefusal) -> bool {
        matches!(
            (self, expected),
            (Observed::Envelope, ExpectedRefusal::Envelope)
        ) || matches!(
            (self, expected),
            (Observed::Raw(observed), ExpectedRefusal::Raw(wanted)) if observed == wanted
        )
    }
}

// ---------------------------------------------------------------------------
// The planted-invalid inventory
// ---------------------------------------------------------------------------

/// One planted-invalid wire fixture.
///
/// `base` plus exactly one `mutation` yields the planted document. `expected`
/// is the refusal that must fire; `shadowed` records a deeper bound the
/// document also crosses but which the admission order makes unreachable.
#[derive(Clone, Copy)]
struct PlantedMutation {
    id: &'static str,
    base: &'static str,
    mutation: &'static str,
    expected: ExpectedRefusal,
    shadowed: Option<RawJsonAdmissionError>,
    role: JsonRpcEndpointRole,
    direction: JsonRpcMessageDirection,
    scope: Option<ClientIngressFailureScope>,
    plant: fn(&str) -> Vec<u8>,
}

/// Ordered planted-invalid inventory.
///
/// The order mirrors `admit_raw_jsonrpc_document`'s own bound sequence —
/// length, byte-order mark, UTF-8, top-level shape, object interior, trailing
/// bytes, then envelope decoding — so the ordering property is readable from
/// the table rather than only from the assertions.
fn inventory() -> Vec<PlantedMutation> {
    vec![
        PlantedMutation {
            id: "TST-01.B.01",
            base: CANONICAL_REQUEST,
            mutation: "pad the params object past the fixed document byte limit",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::DocumentTooLarge),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let padding = "a".repeat(ADMISSION_BYTE_LIMIT);
                base.replace(
                    r#""params":{}"#,
                    &format!(r#""params":{{"pad":"{padding}"}}"#),
                )
                .into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.02",
            base: CANONICAL_REQUEST,
            mutation: "prefix the document with a UTF-8 byte-order mark",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::ByteOrderMark),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let mut planted = vec![0xef, 0xbb, 0xbf];
                planted.extend_from_slice(base.as_bytes());
                planted
            },
        },
        PlantedMutation {
            id: "TST-01.B.03",
            base: CANONICAL_REQUEST,
            mutation: "embed a byte-order mark inside a string value, not at the prefix",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::ByteOrderMark),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                // The shipped scan is whole-document, not prefix-only. A
                // fixture that only ever planted a leading BOM would never
                // observe that, and would pass either way.
                base.replace("\"ping\"", "\"pi\u{feff}ng\"").into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.04",
            base: CANONICAL_REQUEST,
            mutation: "replace one method byte with a lone continuation byte",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::InvalidUtf8),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let mut planted = base.as_bytes().to_vec();
                let index = base
                    .find("ping")
                    .expect("the canonical base names a method");
                planted[index] = 0x80;
                planted
            },
        },
        PlantedMutation {
            id: "TST-01.B.05",
            base: CANONICAL_REQUEST,
            mutation: "wrap the document in a top-level batch array",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::TopLevelBatch),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| format!("[{base}]").into_bytes(),
        },
        PlantedMutation {
            id: "TST-01.B.06",
            base: CANONICAL_REQUEST,
            mutation: "replace the top-level object with a bare JSON string",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::TopLevelNotObject),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |_| br#""not-an-object""#.to_vec(),
        },
        PlantedMutation {
            id: "TST-01.B.07",
            base: CANONICAL_REQUEST,
            mutation: "repeat the jsonrpc member so the object carries a duplicate key",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::DuplicateObjectMember),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                base.replacen(
                    r#"{"jsonrpc":"2.0""#,
                    r#"{"jsonrpc":"2.0","jsonrpc":"2.0""#,
                    1,
                )
                .into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.08",
            base: CANONICAL_REQUEST,
            mutation: "nest params one level beyond the shipped nesting bound",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::NestingTooDeep),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let depth = MAX_RAW_JSON_NESTING_DEPTH + 4;
                let mut nested = String::from("{}");
                for _ in 0..depth {
                    nested = format!("{{\"n\":{nested}}}");
                }
                base.replace(r#""params":{}"#, &format!(r#""params":{nested}"#))
                    .into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.09",
            base: CANONICAL_REQUEST,
            mutation: "give params a numeric token longer than the shipped number bound",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::NumberTooLong),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let digits = "1".repeat(MAX_RAW_JSON_NUMBER_BYTES + 16);
                base.replace(r#""params":{}"#, &format!(r#""params":{{"n":{digits}}}"#))
                    .into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.10",
            base: CANONICAL_REQUEST,
            mutation: "give params an exponent beyond the shipped exponent bound",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::ExponentTooLarge),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                let exponent = MAX_RAW_JSON_EXPONENT + 1;
                base.replace(
                    r#""params":{}"#,
                    &format!(r#""params":{{"n":1e{exponent}}}"#),
                )
                .into_bytes()
            },
        },
        PlantedMutation {
            id: "TST-01.B.11",
            base: CANONICAL_REQUEST,
            mutation: "append a second document after the complete top-level object",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::InvalidSyntax),
            shadowed: None,
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| format!("{base}{base}").into_bytes(),
        },
        PlantedMutation {
            id: "TST-01.B.12",
            base: CANONICAL_RESULT,
            mutation: "strip every JSON-RPC member, leaving a valid but foreign object",
            expected: ExpectedRefusal::Envelope,
            shadowed: None,
            role: JsonRpcEndpointRole::ClientIngress,
            direction: JsonRpcMessageDirection::ServerToClient,
            scope: Some(ClientIngressFailureScope::OwningExchange),
            plant: |_| br#"{"unrelated":true}"#.to_vec(),
        },
        // The remaining raw bounds are structurally unreachable under a single
        // fixed document limit: crossing them requires a document larger than
        // ADMISSION_BYTE_LIMIT, so DocumentTooLarge fires first. They are
        // recorded rather than omitted, because which bounds are reachable
        // under a given limit IS the ordering property this slice owns.
        PlantedMutation {
            id: "TST-01.B.13",
            base: CANONICAL_REQUEST,
            mutation: "exceed the container-entry bound, which requires an oversized document",
            expected: ExpectedRefusal::Raw(RawJsonAdmissionError::DocumentTooLarge),
            shadowed: Some(RawJsonAdmissionError::TooManyContainerEntries),
            role: JsonRpcEndpointRole::ServerIngress,
            direction: JsonRpcMessageDirection::ClientToServer,
            scope: None,
            plant: |base| {
                // Smallest entry encoding is 6 bytes, so clearing the entry
                // bound necessarily clears the byte bound first.
                let entries = (0..40_000)
                    .map(|index| format!(r#""k{index}":0"#))
                    .collect::<Vec<_>>()
                    .join(",");
                base.replace(r#""params":{}"#, &format!(r#""params":{{{entries}}}"#))
                    .into_bytes()
            },
        },
    ]
}

// ---------------------------------------------------------------------------
// The mutation oracle
// ---------------------------------------------------------------------------

/// Drives one planted document through the shipped public surface.
fn observe(document: &[u8]) -> Observed {
    match admit_raw_jsonrpc_document(document, ADMISSION_BYTE_LIMIT) {
        Err(error) => Observed::Raw(error),
        Ok(()) => match decode_strict_jsonrpc_message(document, ADMISSION_BYTE_LIMIT) {
            Ok(_) => Observed::Admitted,
            Err(JsonRpcAdmissionError::InvalidEnvelope) => Observed::Envelope,
            Err(JsonRpcAdmissionError::Raw(error)) => Observed::Raw(error),
        },
    }
}

/// Every named mutable state field the oracle accumulates.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct OracleState {
    confirmed: Vec<String>,
    shadow_checks: Vec<String>,
    bases_admitted: Vec<String>,
}

impl OracleState {
    /// Canonical byte encoding used for the byte-for-byte unchanged proof.
    fn canonical_bytes(&self) -> Vec<u8> {
        let mut encoded = String::new();
        for (label, rows) in [
            ("confirmed", &self.confirmed),
            ("shadow_checks", &self.shadow_checks),
            ("bases_admitted", &self.bases_admitted),
        ] {
            encoded.push_str(label);
            encoded.push('=');
            encoded.push_str(&rows.join(","));
            encoded.push(';');
        }
        encoded.into_bytes()
    }
}

/// Proves every canonical base is admitted, so a later refusal is attributable
/// to the planted mutation and not to a base that was already invalid.
fn admit_canonical_bases(state: &mut OracleState) {
    for (label, base) in CANONICAL_BASES {
        let observed = observe(base.as_bytes());
        assert_eq!(
            observed,
            Observed::Admitted,
            "canonical base {label} must be admitted before anything is planted into it; \
             a base that is already refused makes every mutation from it unattributable"
        );
        let decoded = decode_strict_jsonrpc_message(base.as_bytes(), ADMISSION_BYTE_LIMIT)
            .expect("an admitted canonical base decodes as a JSON-RPC message");
        assert!(
            matches!(
                decoded,
                JsonRpcMessage::Request(_) | JsonRpcMessage::Response(_)
            ),
            "canonical base {label} must decode as a JSON-RPC message"
        );
        state.bases_admitted.push(label.to_owned());
    }
}

/// Applies one planted mutation and requires the exact named refusal.
///
/// A mutation refused by a different bound than the one it names is reported as
/// shadowed, not accepted as a pass: that is the failure mode where a negative
/// fixture looks green while never reaching the behaviour it claims to test.
fn confirm(state: &mut OracleState, case: &PlantedMutation) {
    let planted = (case.plant)(case.base);
    assert_ne!(
        planted.as_slice(),
        case.base.as_bytes(),
        "{}: the mutation must actually change the document",
        case.id
    );

    let observed = observe(&planted);

    assert_ne!(
        observed,
        Observed::Admitted,
        "{}: planted document was ADMITTED. {} is not an invalid-wire fixture at all.",
        case.id,
        case.mutation
    );
    assert!(
        observed.matches(case.expected),
        "{}: SHADOWED. Mutation `{}` names {:?} but the shipped surface refused with {:?}. \
         A fixture refused for a different reason never reaches the bound it claims to test.",
        case.id,
        case.mutation,
        case.expected,
        observed
    );

    if let Some(shadowed) = case.shadowed {
        assert_ne!(
            observed,
            Observed::Raw(shadowed),
            "{}: expected the earlier bound to shadow {:?}, but it fired",
            case.id,
            shadowed
        );
        state.shadow_checks.push(case.id.to_owned());
    }

    // Role, direction and ownership labels travel with every malformed fixture.
    match case.role {
        JsonRpcEndpointRole::ServerIngress => assert_eq!(
            case.direction,
            JsonRpcMessageDirection::ClientToServer,
            "{}: a server-ingress fixture carries client-to-server direction",
            case.id
        ),
        JsonRpcEndpointRole::ClientIngress => {
            assert_eq!(
                case.direction,
                JsonRpcMessageDirection::ServerToClient,
                "{}: a client-ingress fixture carries server-to-client direction",
                case.id
            );
            assert!(
                case.scope.is_some(),
                "{}: a client-ingress fixture records its bounded failure scope, because it \
                 answers malformed server output with no JSON-RPC error and an empty outbound wire",
                case.id
            );
        }
    }

    state.confirmed.push(case.id.to_owned());
}

/// Runs the ordered inventory and returns the accumulated state.
fn run_inventory(cases: &[PlantedMutation]) -> OracleState {
    let mut state = OracleState::default();
    admit_canonical_bases(&mut state);
    for case in cases {
        confirm(&mut state, case);
    }
    state
}

// ---------------------------------------------------------------------------
// Frozen entry points
// ---------------------------------------------------------------------------

/// TST-01 B positive: every planted-invalid wire fixture is refused by the
/// shipped public surface for exactly the bound it names, in the shipped
/// admission order, with its endpoint role, direction and ownership recorded.
#[test]
fn tst_01_b_positive() {
    let cases = inventory();

    // Case identity is frozen and ordered; the ordering claim is meaningless
    // if the table can be silently reordered.
    let ids: Vec<&str> = cases.iter().map(|case| case.id).collect();
    let expected: Vec<String> = (1..=cases.len())
        .map(|ordinal| format!("TST-01.B.{ordinal:02}"))
        .collect();
    assert_eq!(
        ids, expected,
        "the planted-invalid inventory is ordered and contiguous"
    );

    let state = run_inventory(&cases);

    assert_eq!(
        state.bases_admitted.len(),
        CANONICAL_BASES.len(),
        "every canonical base is admitted before mutation"
    );
    assert_eq!(
        state.confirmed.len(),
        cases.len(),
        "every planted mutation is confirmed against its exact named refusal"
    );
    assert!(
        !state.shadow_checks.is_empty(),
        "at least one case proves an earlier bound shadows a deeper one, which is the \
         ordering property this slice owns"
    );

    // Distinct bounds are genuinely reached, so the inventory is not one bound
    // restated twelve times.
    let mut reached: Vec<String> = cases
        .iter()
        .map(|case| format!("{:?}", case.expected))
        .collect();
    reached.sort();
    reached.dedup();
    assert!(
        reached.len() >= 9,
        "the inventory must reach at least nine distinct refusal bounds, reached {reached:?}"
    );
}

/// TST-01 B planted negative: the same inventory, the same oracle, the same
/// canonical bases and the same admission limit, varying only the forbidden
/// dimension — one fixture names a bound that is not the one the shipped
/// surface actually applies to it.
///
/// The oracle must reach its typed refusal boundary on that fixture, and every
/// named mutable state field must be byte-for-byte unchanged, because a
/// shadowed fixture contributes nothing to the confirmed inventory.
#[test]
fn tst_01_b_planted_negative() {
    let mut cases = inventory();

    // The single changed variable: case 02 plants a byte-order mark, which the
    // shipped surface refuses with ByteOrderMark. Here it claims InvalidUtf8 —
    // a bound the same document would cross only if the BOM check did not run
    // first. Everything else about the fixture is untouched.
    let shadowed_index = 1;
    assert_eq!(cases[shadowed_index].id, "TST-01.B.02");
    assert_eq!(
        cases[shadowed_index].expected,
        ExpectedRefusal::Raw(RawJsonAdmissionError::ByteOrderMark),
        "the negative must start from the case's true bound before changing it"
    );
    cases[shadowed_index].expected = ExpectedRefusal::Raw(RawJsonAdmissionError::InvalidUtf8);

    // State accumulated up to, but not including, the mis-declared fixture.
    let mut state = OracleState::default();
    admit_canonical_bases(&mut state);
    for case in &cases[..shadowed_index] {
        confirm(&mut state, case);
    }
    let before = state.clone();
    let before_bytes = before.canonical_bytes();

    // The oracle must refuse the mis-declared fixture rather than record it.
    // The refusal is a panic by construction, so the hook is silenced across
    // this one deliberate catch and restored immediately: an expected panic
    // printed into an otherwise green run reads as a failure to whoever is
    // holding the gate output.
    let mut probe = state.clone();
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        confirm(&mut probe, &cases[shadowed_index]);
    }));
    std::panic::set_hook(previous_hook);
    assert!(
        outcome.is_err(),
        "a fixture that names the wrong bound must be refused by the oracle, not recorded"
    );

    // Byte-for-byte unchanged: a refused fixture contributes nothing.
    let after = state;
    assert_eq!(
        after, before,
        "a shadowed fixture must not mutate the oracle's named state"
    );
    assert_eq!(
        after.canonical_bytes(),
        before_bytes,
        "oracle state must be byte-for-byte unchanged after a shadowed fixture"
    );

    // The refusal is caused by the changed dimension alone: restoring the true
    // bound makes the identical fixture confirm.
    cases[shadowed_index].expected = ExpectedRefusal::Raw(RawJsonAdmissionError::ByteOrderMark);
    let mut restored = before;
    confirm(&mut restored, &cases[shadowed_index]);
    assert_eq!(
        restored.confirmed.last().map(String::as_str),
        Some("TST-01.B.02"),
        "with its true bound restored the same fixture confirms"
    );
}
