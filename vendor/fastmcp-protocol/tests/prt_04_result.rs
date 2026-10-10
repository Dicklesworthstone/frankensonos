//! PRT-04 result algebra: the frozen public-surface proofs.
//!
//! Every case here consumes `fastmcp_protocol` as a downstream user does. None
//! of it can see a `cfg(test)` path, so what passes here is what ships.

use std::collections::BTreeMap;

use fastmcp_core::crypto::sha256_bounded;
use fastmcp_protocol::{
    CompleteResultPayload, CoreResultDiscriminatorPolicy, DecodedResult,
    DeferringResultDiscriminatorPolicy, ExactJsonMember, ExactJsonObject, ExactJsonValue,
    MAX_PRT_04_MANIFEST_BYTES, PRT_04_A_EVALUATOR_MANIFEST_V1, PRT_04_B_EVALUATOR_MANIFEST_V1,
    ResultDecodeError, ResultDecodeErrorKind, ResultMeta, ResultPeerDiagnostic, ResultPeerEra,
    TypedCompleteMembers, UnknownResultMembers, decode_peer_result, decode_typed_complete,
    encode_complete_result, encode_result, prt_04_a_manifest_digest, prt_04_b_manifest_digest,
};

#[derive(Debug, PartialEq, Eq)]
struct LookupResult {
    status: String,
    record: ExactJsonObject,
}

impl CompleteResultPayload for LookupResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] = &["status", "record"];

    fn decode_known_members(
        members: &mut TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let Some(ExactJsonValue::String(status)) = members.take("status")? else {
            return Err(ResultDecodeError::invalid_known_member("$.status"));
        };
        let Some(ExactJsonValue::Object(record)) = members.take("record")? else {
            return Err(ResultDecodeError::invalid_known_member("$.record"));
        };
        Ok(Self { status, record })
    }
}

// ---------------------------------------------------------------------------
// Shipped-manifest binding
// ---------------------------------------------------------------------------

/// One parsed `<id> <name> floor=<N>` row of the shipped A manifest.
#[derive(Debug)]
struct ManifestCase {
    id: String,
    name: String,
    floor: usize,
}

/// Counts the observations this file actually performs, keyed by case name.
///
/// There is deliberately no local floor table. The minimum for each case is
/// read from the bytes `fastmcp-protocol` publishes, so lowering a floor to buy
/// green visibly weakens the producer's own shipped acceptance input and
/// changes `prt_04_a_manifest_digest()`, while raising one without adding an
/// observation turns this test red on the next run.
#[derive(Debug, Default)]
struct Observations {
    counts: BTreeMap<String, usize>,
}

impl Observations {
    fn observe(&mut self, case: &str) {
        *self.counts.entry(case.to_owned()).or_default() += 1;
    }

    /// Fails unless the executed case set is exactly the published case set and
    /// every published floor was met by real observations.
    fn assert_meets(&self, cases: &[ManifestCase]) {
        let published: Vec<&str> = cases.iter().map(|case| case.name.as_str()).collect();
        let executed: Vec<&str> = self.counts.keys().map(String::as_str).collect();
        let mut sorted_published = published.clone();
        sorted_published.sort_unstable();
        assert_eq!(
            executed, sorted_published,
            "the executed case set must be exactly the published case set: \
             an unpublished case earns no credit and a published case that \
             never runs is a zero-run row"
        );
        for case in cases {
            let observed = self.counts.get(&case.name).copied().unwrap_or_default();
            assert!(
                observed >= case.floor,
                "{} ({}) declares floor={} but only {} observation(s) executed",
                case.id,
                case.name,
                case.floor,
                observed
            );
        }
    }
}

/// Parses and checks the `prt_04_evaluator_manifest_v1` rows this slice ships.
///
/// The manifest is deliberately not rebuilt here.
/// `PRT_04_A_EVALUATOR_MANIFEST_V1` is the producer-owned acceptance input the
/// PRT-04 integration join consumes; a locally authored copy would prove
/// nothing about what ships. This asserts the published bytes are LF-canonical,
/// that their case ids are contiguous `PRT-04.NN` from `01` in frozen order,
/// that every row declares a positive observation floor, and that the published
/// digest still binds the published bytes.
fn assert_shipped_a_manifest() -> Vec<ManifestCase> {
    let text = PRT_04_A_EVALUATOR_MANIFEST_V1;
    assert!(
        text.ends_with('\n') && !text.contains('\r'),
        "the published manifest must be LF-canonical and LF-terminated"
    );
    let mut lines = text.split('\n');
    let mut rows: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        assert_eq!(
            line.trim_end(),
            line,
            "manifest rows must carry no trailing whitespace"
        );
        rows.push(line);
    }
    assert!(
        lines.next().is_none(),
        "the manifest must contain no blank or trailing line"
    );
    assert!(
        rows.len() > 4,
        "the manifest is four header rows plus at least one case row"
    );
    assert_eq!(rows[0], "PRT-04-A evaluator manifest v1");
    assert!(rows[1].starts_with("producer-revision "));
    assert!(rows[2].starts_with("producer-tree "));
    assert!(
        rows[3]
            .strip_prefix("entrypoint ")
            .is_some_and(|entrypoint| entrypoint.starts_with("fastmcp")),
        "the manifest must name a shipped public entrypoint"
    );

    let mut cases = Vec::new();
    for (index, row) in rows[4..].iter().enumerate() {
        let fields: Vec<&str> = row.split(' ').collect();
        assert_eq!(
            fields.len(),
            3,
            "case row {index} must be `<id> <name> floor=<N>`"
        );
        assert_eq!(
            fields[0],
            format!("PRT-04.{:02}", index + 1),
            "the published A case ids are contiguous and frozen in order"
        );
        assert!(!fields[1].is_empty(), "case row {index} must name its case");
        let floor: usize = fields[2]
            .strip_prefix("floor=")
            .expect("each case row declares `floor=<N>`")
            .parse()
            .expect("each floor is numeric");
        assert!(floor >= 1, "case row {index} must declare a positive floor");
        cases.push(ManifestCase {
            id: fields[0].to_owned(),
            name: fields[1].to_owned(),
            floor,
        });
    }

    // The published digest must bind the published bytes; a digest that no
    // longer recomputes means the rows and the digest have drifted apart.
    let recomputed = sha256_bounded(text.as_bytes(), MAX_PRT_04_MANIFEST_BYTES)
        .expect("the fixed manifest is within its byte bound");
    assert_eq!(
        prt_04_a_manifest_digest().as_bytes(),
        recomputed.as_bytes(),
        "the published PRT-04 A digest must bind the published manifest bytes"
    );
    cases
}

/// Names of the members a decoded envelope retained as inert open siblings.
fn extra_names(extras: &UnknownResultMembers) -> Vec<&str> {
    extras
        .members()
        .iter()
        .map(|member| member.name.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// Frozen entry points
// ---------------------------------------------------------------------------

/// A complete envelope whose open siblings cover every JSON kind, in an order
/// that is deliberately not alphabetical.
///
/// Both member orderings this repo carries appear here: the envelope's own
/// members and `obj`'s members are admitted-order, and a decoder that round
/// trips either through a `serde_json::Value` map would re-emit them sorted and
/// fail the byte-faithful re-encode below.
const OPEN_KINDS_COMPLETE: &str = concat!(
    r#"{"resultType":"complete","#,
    r#""zeta":null,"#,
    r#""alpha":true,"#,
    r#""mid":"text","#,
    r#""num":123456789012345678901234567890,"#,
    r#""arr":[1.20e+4,null],"#,
    r#""obj":{"inner":-0.0,"before":1}}"#,
);

/// An input-required envelope carrying three standard names that belong to a
/// different result composition. They must stay inert.
const FOREIGN_NAMES_INPUT_REQUIRED: &str = concat!(
    r#"{"resultType":"input_required","requestState":"retry-1","#,
    r#""ttlMs":60000,"cacheScope":"public","nextCursor":"opaque-1"}"#,
);

/// A structurally valid envelope carrying a discriminator no core decoder owns.
const UNCLAIMED_DISCRIMINATOR: &str =
    r#"{"resultType":"x.example/stream","cursor":"c-1","payload":{"n":10}}"#;

#[test]
fn prt_04_a_positive() {
    let cases = assert_shipped_a_manifest();
    let mut seen = Observations::default();

    // PRT-04.01 — both core discriminators select their core decoder.
    let (complete, _) = decode_peer_result(
        r#"{"resultType":"complete","extension":{"integer":123456789012345678901234567890}}"#,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("a bounded complete result is admitted");
    assert!(matches!(complete, DecodedResult::Complete(_)));
    seen.observe("core-discriminator-selection");
    let (input_required, _) = decode_peer_result(
        r#"{"resultType":"input_required","requestState":"retry-0"}"#,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("a bounded input-required result is admitted");
    let DecodedResult::InputRequired(input_required) = input_required else {
        panic!("input_required selects the input-required decoder");
    };
    assert_eq!(input_required.request_state(), Some("retry-0"));
    seen.observe("core-discriminator-selection");

    // PRT-04.02 — an absent discriminator defaults to complete in either era,
    // and only the modern omission records a peer-nonconformance diagnostic.
    let absent = r#"{"resultType-lookalike":"complete","note":"no discriminator"}"#;
    let (legacy, legacy_diagnostic) =
        decode_peer_result(absent, ResultPeerEra::Legacy, &CoreResultDiscriminatorPolicy)
            .expect("an earlier-era peer may omit resultType");
    assert!(matches!(legacy, DecodedResult::Complete(_)));
    assert_eq!(legacy_diagnostic, None);
    seen.observe("absent-discriminator-defaults-complete");
    let (modern, modern_diagnostic) =
        decode_peer_result(absent, ResultPeerEra::Modern, &CoreResultDiscriminatorPolicy)
            .expect("a final-era peer omission still decodes as complete");
    assert!(matches!(modern, DecodedResult::Complete(_)));
    assert_eq!(
        modern_diagnostic,
        Some(ResultPeerDiagnostic::ModernMissingResultType),
        "the compatibility default must not silently excuse a final peer"
    );
    seen.observe("absent-discriminator-defaults-complete");

    // PRT-04.03 — a present discriminator of the wrong JSON kind is refused at
    // the structural boundary rather than guessed at or conflated with absence.
    // It never reaches the policy seam, so it carries no raw envelope.
    for wrong_kind in [
        r#"{"resultType":null,"note":"x"}"#,
        r#"{"resultType":7,"note":"x"}"#,
        r#"{"resultType":true,"note":"x"}"#,
    ] {
        let error = decode_peer_result(
            wrong_kind,
            ResultPeerEra::Modern,
            &CoreResultDiscriminatorPolicy,
        )
        .expect_err("a non-string resultType is never guessed at");
        assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidDiscriminator);
        assert_eq!(error.path(), "$.resultType");
        assert!(
            error.raw_envelope().is_none(),
            "a structurally refused discriminator never reached the policy seam"
        );
        seen.observe("nonstring-discriminator-refused");
    }

    // PRT-04.04 — the sealed seam reports all three decisions. The same bytes
    // are core, deferred, or rejected purely by which policy is injected; the
    // codec itself never decides to activate anything.
    let (core, _) = decode_peer_result(
        r#"{"resultType":"complete","k":1}"#,
        ResultPeerEra::Modern,
        &DeferringResultDiscriminatorPolicy,
    )
    .expect("a core discriminator stays core under either policy");
    assert!(matches!(core, DecodedResult::Complete(_)));
    seen.observe("policy-seam-core-deferred-rejected");
    let (deferred, _) = decode_peer_result(
        UNCLAIMED_DISCRIMINATOR,
        ResultPeerEra::Modern,
        &DeferringResultDiscriminatorPolicy,
    )
    .expect("an unclaimed discriminator is deferred, not decoded");
    seen.observe("policy-seam-core-deferred-rejected");
    let rejected = decode_peer_result(
        UNCLAIMED_DISCRIMINATOR,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect_err("the core policy refuses what it cannot claim");
    assert_eq!(rejected.kind(), ResultDecodeErrorKind::RejectedExtension);
    assert_eq!(rejected.path(), "$.resultType");
    let diagnostic_envelope = rejected
        .raw_envelope()
        .expect("a policy rejection preserves its bounded raw envelope");
    assert_eq!(diagnostic_envelope.discriminator(), "x.example/stream");
    seen.observe("policy-seam-core-deferred-rejected");

    // PRT-04.05 — a deferred envelope is retained, never activated. It carries
    // no typed payload and no core semantics, and re-emitting it reproduces the
    // received bytes, so a proxy can forward what it cannot interpret.
    let DecodedResult::Deferred(envelope) = &deferred else {
        panic!("a deferred decision yields a raw envelope, not a core result");
    };
    assert_eq!(envelope.discriminator(), "x.example/stream");
    seen.observe("deferred-envelope-never-activated");
    assert_eq!(
        envelope
            .members()
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>(),
        ["resultType", "cursor", "payload"],
        "the raw envelope retains every admitted member in admitted order"
    );
    seen.observe("deferred-envelope-never-activated");
    assert!(
        !matches!(
            deferred,
            DecodedResult::Complete(_) | DecodedResult::InputRequired(_)
        ),
        "deferring is not activating"
    );
    assert_eq!(encode_result(&deferred), UNCLAIMED_DISCRIMINATOR);
    seen.observe("deferred-envelope-never-activated");

    // PRT-04.06 — every JSON kind survives as an inert open sibling with its
    // exact value, and the admitted order is preserved rather than sorted.
    let (open_kinds, _) = decode_peer_result(
        OPEN_KINDS_COMPLETE,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("open siblings of every JSON kind are admitted");
    let DecodedResult::Complete(open_kinds) = open_kinds else {
        panic!("complete result");
    };
    assert_eq!(
        extra_names(&open_kinds.extras),
        ["zeta", "alpha", "mid", "num", "arr", "obj"],
        "admitted order is preserved; an alphabetical map round trip would sort these"
    );
    assert_eq!(
        open_kinds.extras.members()[0].value,
        ExactJsonValue::Null,
        "explicit null is a retained value, not an absence"
    );
    seen.observe("open-member-kind-and-order-preservation");
    assert_eq!(
        open_kinds.extras.members()[1].value,
        ExactJsonValue::Bool(true)
    );
    seen.observe("open-member-kind-and-order-preservation");
    assert_eq!(
        open_kinds.extras.members()[2].value,
        ExactJsonValue::String("text".to_owned())
    );
    seen.observe("open-member-kind-and-order-preservation");
    assert_eq!(
        open_kinds.extras.members()[3].value,
        ExactJsonValue::Number("123456789012345678901234567890".to_owned()),
        "an arbitrary-precision integer keeps its source lexeme, not an f64"
    );
    seen.observe("open-member-kind-and-order-preservation");
    assert_eq!(
        open_kinds.extras.members()[4].value,
        ExactJsonValue::Array(vec![
            ExactJsonValue::Number("1.20e+4".to_owned()),
            ExactJsonValue::Null,
        ]),
        "the exponent lexeme `1.20e+4` is retained verbatim"
    );
    seen.observe("open-member-kind-and-order-preservation");
    let ExactJsonValue::Object(nested) = &open_kinds.extras.members()[5].value else {
        panic!("a nested object is retained as an object");
    };
    assert_eq!(
        nested
            .members()
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>(),
        ["inner", "before"],
        "nested member order is preserved too"
    );
    assert_eq!(
        nested.get("inner"),
        Some(&ExactJsonValue::Number("-0.0".to_owned())),
        "negative zero is not normalised away"
    );
    seen.observe("open-member-kind-and-order-preservation");

    // PRT-04.07 — re-emission is byte-faithful for both core compositions. A
    // peer or proxy must not discard or semantically rewrite a member.
    assert_eq!(
        encode_result(&DecodedResult::Complete(open_kinds)),
        OPEN_KINDS_COMPLETE
    );
    seen.observe("open-member-byte-faithful-reencode");
    let (foreign, _) = decode_peer_result(
        FOREIGN_NAMES_INPUT_REQUIRED,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("input-required envelopes admit open siblings too");
    let DecodedResult::InputRequired(foreign) = foreign else {
        panic!("input-required result");
    };
    assert_eq!(
        encode_result(&DecodedResult::InputRequired(foreign.clone())),
        FOREIGN_NAMES_INPUT_REQUIRED
    );
    seen.observe("open-member-byte-faithful-reencode");

    // PRT-04.08 — a common name can never fall back into the open-member set.
    // Decoding consumes it as the common field, and the locally authored extras
    // constructor refuses it outright.
    let (common, _) = decode_peer_result(
        r#"{"resultType":"complete","_meta":{"trace":true},"serverInfo":{"name":"FastMCP","version":"0.1"},"other":1}"#,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("common members decode into their common slots");
    let DecodedResult::Complete(common) = common else {
        panic!("complete result");
    };
    assert_eq!(
        extra_names(&common.extras),
        ["other"],
        "no common name is demoted into the open-member set"
    );
    assert!(matches!(
        common.meta.metadata().get("trace"),
        Some(ExactJsonValue::Bool(true))
    ));
    assert_eq!(
        common.meta.server_info.as_ref().map(|info| info.name.as_str()),
        Some("FastMCP")
    );
    for common_name in ["resultType", "_meta", "serverInfo"] {
        let collision = UnknownResultMembers::try_new(
            vec![ExactJsonMember {
                name: common_name.to_owned(),
                value: ExactJsonValue::Bool(true),
            }],
            &[],
        )
        .expect_err("a locally authored extra cannot borrow a common name");
        assert_eq!(
            collision.kind(),
            ResultDecodeErrorKind::KnownMemberCollision
        );
        assert_eq!(collision.path(), common_name);
        seen.observe("common-name-never-demoted-to-extras");
    }

    // PRT-04.09 — standard names owned by another composition are retained as
    // explicitly inert siblings. They must not borrow that composition's cache,
    // pagination, or routing semantics on an input-required result.
    assert_eq!(
        extra_names(&foreign.extras),
        ["ttlMs", "cacheScope", "nextCursor"],
        "foreign standard names are retained, in order, as open siblings"
    );
    assert_eq!(foreign.input_requests(), None);
    assert_eq!(foreign.request_state(), Some("retry-1"));
    assert_eq!(
        foreign.extras.members()[0].value,
        ExactJsonValue::Number("60000".to_owned()),
        "`ttlMs` on an input-required result is data, not a cache hint"
    );
    seen.observe("foreign-composition-names-inert");
    assert_eq!(
        foreign.extras.members()[1].value,
        ExactJsonValue::String("public".to_owned()),
        "`cacheScope` here cannot mint a shareable result"
    );
    seen.observe("foreign-composition-names-inert");
    assert_eq!(
        foreign.extras.members()[2].value,
        ExactJsonValue::String("opaque-1".to_owned()),
        "`nextCursor` here cannot select a pagination continuation"
    );
    seen.observe("foreign-composition-names-inert");

    seen.assert_meets(&cases);
}

#[test]
fn prt_04_a_planted_negative() {
    // Named mutable state, captured before any planted input is offered. Each
    // field is compared byte-for-byte after every refusal below.
    let cases = assert_shipped_a_manifest();
    let published_manifest_bytes = PRT_04_A_EVALUATOR_MANIFEST_V1.to_owned();
    let published_digest = prt_04_a_manifest_digest();
    let (baseline, baseline_diagnostic) = decode_peer_result(
        OPEN_KINDS_COMPLETE,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("baseline complete result");
    let DecodedResult::Complete(baseline) = baseline else {
        panic!("complete baseline");
    };
    let baseline_extras = baseline.extras.clone();
    let baseline_bytes = encode_result(&DecodedResult::Complete(baseline));
    assert_eq!(baseline_bytes, OPEN_KINDS_COMPLETE);
    assert_eq!(baseline_diagnostic, None);

    // Planted mutation 1 — the forbidden dimension is the discriminator's JSON
    // kind. Exactly one input dimension differs from the accepted baseline:
    // `"complete"` becomes `null`. Every open sibling is byte-identical.
    let planted_discriminator = OPEN_KINDS_COMPLETE.replacen(
        r#""resultType":"complete""#,
        r#""resultType":null"#,
        1,
    );
    assert_ne!(planted_discriminator, OPEN_KINDS_COMPLETE);
    let error = decode_peer_result(
        &planted_discriminator,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect_err("only the resultType JSON kind changed");
    assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidDiscriminator);
    assert_eq!(error.path(), "$.resultType");
    assert!(
        error.raw_envelope().is_none(),
        "a refused discriminator must not hand back an activatable envelope"
    );

    // Planted mutation 2 — the forbidden dimension is member uniqueness. One
    // open sibling name is duplicated; nothing else differs.
    let planted_duplicate =
        OPEN_KINDS_COMPLETE.replacen(r#""alpha":true"#, r#""alpha":true,"zeta":1"#, 1);
    assert_ne!(planted_duplicate, OPEN_KINDS_COMPLETE);
    let error = decode_peer_result(
        &planted_duplicate,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect_err("duplicate-aware admission refuses a repeated member name");
    assert_eq!(error.kind(), ResultDecodeErrorKind::DuplicateMember);

    // Planted mutation 3 — the forbidden dimension is which policy is injected.
    // The bytes are identical to the accepted deferral; only the seam changes,
    // and the refusal must still preserve the bounded envelope for diagnostics
    // without ever activating it.
    let (accepted_deferral, _) = decode_peer_result(
        UNCLAIMED_DISCRIMINATOR,
        ResultPeerEra::Modern,
        &DeferringResultDiscriminatorPolicy,
    )
    .expect("baseline deferral");
    assert!(matches!(accepted_deferral, DecodedResult::Deferred(_)));
    let error = decode_peer_result(
        UNCLAIMED_DISCRIMINATOR,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect_err("only the injected policy changed");
    assert_eq!(error.kind(), ResultDecodeErrorKind::RejectedExtension);
    assert_eq!(
        error
            .raw_envelope()
            .expect("the rejection preserves its raw envelope")
            .discriminator(),
        "x.example/stream"
    );

    // Named mutable state, byte-for-byte unchanged across all three refusals.
    let (reaccepted, reaccepted_diagnostic) = decode_peer_result(
        OPEN_KINDS_COMPLETE,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("a refusal cannot poison a later accepted result");
    let DecodedResult::Complete(reaccepted) = reaccepted else {
        panic!("complete result");
    };
    assert_eq!(reaccepted.extras, baseline_extras);
    assert_eq!(reaccepted_diagnostic, baseline_diagnostic);
    assert_eq!(
        encode_result(&DecodedResult::Complete(reaccepted)),
        baseline_bytes
    );
    assert_eq!(PRT_04_A_EVALUATOR_MANIFEST_V1, published_manifest_bytes);
    assert_eq!(
        prt_04_a_manifest_digest().as_bytes(),
        published_digest.as_bytes()
    );
    assert_eq!(assert_shipped_a_manifest().len(), cases.len());
}

// ---------------------------------------------------------------------------
// PRT-04 B — typed decode, unknown preservation, rejection
// ---------------------------------------------------------------------------

/// Declares three members but deliberately consumes only two, so the residual
/// declared name proves a selected-known field cannot be demoted into extras.
#[derive(Debug, PartialEq, Eq)]
struct PartialLookupResult {
    status: String,
}

impl CompleteResultPayload for PartialLookupResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] = &["status", "record", "audit"];

    fn decode_known_members(
        members: &mut TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let Some(ExactJsonValue::String(status)) = members.take("status")? else {
            return Err(ResultDecodeError::invalid_known_member("$.status"));
        };
        let _ = members.take("record")?;
        // `audit` is declared and never consumed. The codec must refuse rather
        // than let a declared name slip into the inert open-member set.
        Ok(Self { status })
    }
}

/// Reaches for an open member it never declared.
#[derive(Debug)]
struct UndeclaredGrabResult;

impl CompleteResultPayload for UndeclaredGrabResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] = &["status"];

    fn decode_known_members(
        members: &mut TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let _ = members.take("opaque")?;
        Ok(Self)
    }
}

/// Reaches for a common member it never declared.
#[derive(Debug)]
struct CommonGrabResult;

impl CompleteResultPayload for CommonGrabResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] = &["status"];

    fn decode_known_members(
        members: &mut TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let _ = members.take("_meta")?;
        Ok(Self)
    }
}

/// Declares a common name as if it were method-specific.
#[derive(Debug)]
struct CommonNameClaimingResult;

impl CompleteResultPayload for CommonNameClaimingResult {
    const KNOWN_MEMBER_NAMES: &'static [&'static str] = &["status", "_meta"];

    fn decode_known_members(
        members: &mut TypedCompleteMembers<'_>,
    ) -> Result<Self, ResultDecodeError> {
        let _ = members.take("status")?;
        Ok(Self)
    }
}

/// Parses and checks the `prt_04_evaluator_manifest_v1` rows the B slice ships.
///
/// Identical contract to [`assert_shipped_a_manifest`], with the ordinals
/// continuing where the A half stopped. The B half is deliberately checked
/// against A: the ordered union must be contiguous with no gap, overlap, or
/// duplicated case name, because that union is what the PRT-04 integration
/// join consumes.
fn assert_shipped_b_manifest() -> Vec<ManifestCase> {
    let text = PRT_04_B_EVALUATOR_MANIFEST_V1;
    assert!(
        text.ends_with('\n') && !text.contains('\r'),
        "the published manifest must be LF-canonical and LF-terminated"
    );
    let mut lines = text.split('\n');
    let mut rows: Vec<&str> = Vec::new();
    for line in lines.by_ref() {
        if line.is_empty() {
            break;
        }
        assert_eq!(
            line.trim_end(),
            line,
            "manifest rows must carry no trailing whitespace"
        );
        rows.push(line);
    }
    assert!(
        lines.next().is_none(),
        "the manifest must contain no blank or trailing line"
    );
    assert!(
        rows.len() > 4,
        "the manifest is four header rows plus at least one case row"
    );
    assert_eq!(rows[0], "PRT-04-B evaluator manifest v1");
    assert!(rows[1].starts_with("producer-revision "));
    assert!(rows[2].starts_with("producer-tree "));
    assert!(
        rows[3]
            .strip_prefix("entrypoint ")
            .is_some_and(|entrypoint| entrypoint.starts_with("fastmcp")),
        "the manifest must name a shipped public entrypoint"
    );

    let a_cases = assert_shipped_a_manifest();
    let first_b_ordinal = a_cases.len() + 1;
    let mut cases = Vec::new();
    for (index, row) in rows[4..].iter().enumerate() {
        let fields: Vec<&str> = row.split(' ').collect();
        assert_eq!(
            fields.len(),
            3,
            "case row {index} must be `<id> <name> floor=<N>`"
        );
        assert_eq!(
            fields[0],
            format!("PRT-04.{:02}", first_b_ordinal + index),
            "the B ordinals must continue the A half with no gap or overlap"
        );
        assert!(!fields[1].is_empty(), "case row {index} must name its case");
        assert!(
            !a_cases.iter().any(|a_case| a_case.name == fields[1]),
            "case names must be unique across both halves; `{}` is already an A case",
            fields[1]
        );
        let floor: usize = fields[2]
            .strip_prefix("floor=")
            .expect("each case row declares `floor=<N>`")
            .parse()
            .expect("each floor is numeric");
        assert!(floor >= 1, "case row {index} must declare a positive floor");
        cases.push(ManifestCase {
            id: fields[0].to_owned(),
            name: fields[1].to_owned(),
            floor,
        });
    }

    let recomputed = sha256_bounded(text.as_bytes(), MAX_PRT_04_MANIFEST_BYTES)
        .expect("the fixed manifest is within its byte bound");
    assert_eq!(
        prt_04_b_manifest_digest().as_bytes(),
        recomputed.as_bytes(),
        "the published PRT-04 B digest must bind the published manifest bytes"
    );
    assert_ne!(
        prt_04_a_manifest_digest().as_bytes(),
        prt_04_b_manifest_digest().as_bytes(),
        "the two halves are distinct published inputs"
    );
    cases
}

/// A selected `complete` composition in the canonical local layout that
/// `encode_complete_result` emits: discriminator, method-owned members,
/// `_meta`, then inert open siblings. Its unknown siblings cover every JSON
/// kind in a deliberately non-alphabetical order.
const TYPED_CANONICAL_COMPLETE: &str = concat!(
    r#"{"resultType":"complete","#,
    r#""status":"ready","#,
    r#""record":{"id":123456789012345678901234567890},"#,
    r#""_meta":{"trace":true},"#,
    r#""zeta":null,"#,
    r#""alpha":false,"#,
    r#""mid":"text","#,
    r#""num":-1.20e-4,"#,
    r#""arr":[1,"two",null],"#,
    r#""obj":{"inner":0,"before":1}}"#,
);

/// The same composition with no `_meta` at all. A modern result may omit it,
/// and re-encoding must not synthesize it back onto the wire.
/// A frame that is well formed under **both** core compositions.
///
/// Read as `complete`, `requestState` is an inert open sibling. Read as
/// `input_required`, it is that composition's required member. That dual
/// validity is what lets the discriminator be the only thing that differs
/// between an accepted and a refused decode: without it, flipping the
/// discriminator also flips whether the frame is structurally valid for the
/// composition it names, and the refusal arrives from the wrong check.
const DUAL_VALID_COMPOSITION: &str =
    r#"{"resultType":"complete","status":"ready","record":{},"requestState":"retry-1"}"#;

const TYPED_NO_META_COMPLETE: &str = concat!(
    r#"{"resultType":"complete","#,
    r#""status":"ready","#,
    r#""record":{"id":1},"#,
    r#""opaque":{"decimal":1.20e+4}}"#,
);

/// Rebuilds the method-owned members of a decoded [`LookupResult`] in declared
/// order, so a typed decode can be re-encoded through the public codec.
fn lookup_known_members(payload: &LookupResult) -> Vec<ExactJsonMember> {
    vec![
        ExactJsonMember {
            name: "status".to_owned(),
            value: ExactJsonValue::String(payload.status.clone()),
        },
        ExactJsonMember {
            name: "record".to_owned(),
            value: ExactJsonValue::Object(payload.record.clone()),
        },
    ]
}

#[test]
fn prt_04_b_positive() {
    let cases = assert_shipped_b_manifest();
    let mut seen = Observations::default();

    // PRT-04.10 — a selected composition consumes precisely its declared
    // members, with each value validated at its own exact member.
    let (typed, diagnostic) =
        decode_typed_complete::<LookupResult>(TYPED_CANONICAL_COMPLETE, ResultPeerEra::Modern)
            .expect("the public typed result codec admits a bounded complete result");
    assert_eq!(diagnostic, None);
    assert_eq!(typed.payload.status, "ready");
    seen.observe("typed-known-member-selection");
    assert_eq!(
        typed.payload.record.get("id"),
        Some(&ExactJsonValue::Number(
            "123456789012345678901234567890".to_owned()
        )),
        "an arbitrary-precision integer inside a selected member keeps its lexeme"
    );
    seen.observe("typed-known-member-selection");

    // PRT-04.11 — every unknown sibling survives the typed decode with its
    // exact value and its admitted order. This is the defect this slice exists
    // to prevent: a decoder that silently drops what it does not know.
    assert_eq!(
        extra_names(&typed.extras),
        ["zeta", "alpha", "mid", "num", "arr", "obj"],
        "typed decode preserves unknown siblings in admitted order, unsorted"
    );
    assert_eq!(typed.extras.members()[0].value, ExactJsonValue::Null);
    seen.observe("unknown-preserved-through-typed-decode");
    assert_eq!(
        typed.extras.members()[1].value,
        ExactJsonValue::Bool(false),
        "a falsy unknown sibling is retained exactly like a truthy one"
    );
    seen.observe("unknown-preserved-through-typed-decode");
    assert_eq!(
        typed.extras.members()[2].value,
        ExactJsonValue::String("text".to_owned())
    );
    seen.observe("unknown-preserved-through-typed-decode");
    assert_eq!(
        typed.extras.members()[3].value,
        ExactJsonValue::Number("-1.20e-4".to_owned()),
        "a negative exponent lexeme survives without an f64 round trip"
    );
    seen.observe("unknown-preserved-through-typed-decode");
    assert_eq!(
        typed.extras.members()[4].value,
        ExactJsonValue::Array(vec![
            ExactJsonValue::Number("1".to_owned()),
            ExactJsonValue::String("two".to_owned()),
            ExactJsonValue::Null,
        ])
    );
    seen.observe("unknown-preserved-through-typed-decode");
    let ExactJsonValue::Object(nested) = &typed.extras.members()[5].value else {
        panic!("a nested unknown object is retained as an object");
    };
    assert_eq!(
        nested
            .members()
            .iter()
            .map(|member| member.name.as_str())
            .collect::<Vec<_>>(),
        ["inner", "before"],
        "nested unknown member order is preserved through typed decode too"
    );
    seen.observe("unknown-preserved-through-typed-decode");
    for known in LookupResult::KNOWN_MEMBER_NAMES {
        assert!(
            !extra_names(&typed.extras).contains(known),
            "a selected-known name never appears among the retained extras"
        );
    }

    // PRT-04.12 — re-encoding a typed decode reproduces the received bytes.
    // Preservation that cannot be re-emitted is not preservation.
    assert_eq!(
        encode_complete_result(
            &typed.meta,
            lookup_known_members(&typed.payload),
            LookupResult::KNOWN_MEMBER_NAMES,
            &typed.extras,
        )
        .expect("a decoded composition re-encodes through the public codec"),
        TYPED_CANONICAL_COMPLETE
    );
    seen.observe("typed-reencode-byte-faithful");
    let (no_meta, _) =
        decode_typed_complete::<LookupResult>(TYPED_NO_META_COMPLETE, ResultPeerEra::Modern)
            .expect("a modern result may omit _meta entirely");
    assert_eq!(
        encode_complete_result(
            &no_meta.meta,
            lookup_known_members(&no_meta.payload),
            LookupResult::KNOWN_MEMBER_NAMES,
            &no_meta.extras,
        )
        .expect("an absent _meta re-encodes as an absent _meta"),
        TYPED_NO_META_COMPLETE
    );
    seen.observe("typed-reencode-byte-faithful");

    // PRT-04.13 — an invalid or unconsumed selected-known field is a typed
    // failure; it can never be smuggled into the inert open-member set.
    let wrong_kind = TYPED_CANONICAL_COMPLETE.replacen(r#""status":"ready""#, r#""status":false"#, 1);
    let error = decode_typed_complete::<LookupResult>(&wrong_kind, ResultPeerEra::Modern)
        .expect_err("a selected member of the wrong JSON kind fails at that member");
    assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidKnownMember);
    assert_eq!(error.path(), "$.status");
    seen.observe("invalid-known-member-not-smuggled-into-extras");
    let residual = r#"{"resultType":"complete","status":"ready","record":{},"audit":{"who":"x"},"free":1}"#;
    let error = decode_typed_complete::<PartialLookupResult>(residual, ResultPeerEra::Modern)
        .expect_err("a declared but unconsumed member cannot become an extra");
    assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidKnownMember);
    assert_eq!(error.path(), "$.audit");
    seen.observe("invalid-known-member-not-smuggled-into-extras");
    let without_residual = r#"{"resultType":"complete","status":"ready","record":{},"free":1}"#;
    let (partial, _) =
        decode_typed_complete::<PartialLookupResult>(without_residual, ResultPeerEra::Modern)
            .expect("the same composition without the declared member decodes");
    assert_eq!(
        partial.payload.status, "ready",
        "the members it did consume still decoded"
    );
    assert_eq!(
        extra_names(&partial.extras),
        ["free"],
        "only the undeclared sibling is retained"
    );
    seen.observe("invalid-known-member-not-smuggled-into-extras");

    // PRT-04.14 — a payload cannot consume a name it never declared, whether
    // that name is an open sibling or a common member.
    let error = decode_typed_complete::<UndeclaredGrabResult>(
        r#"{"resultType":"complete","status":"ready","opaque":1}"#,
        ResultPeerEra::Modern,
    )
    .expect_err("an undeclared open sibling is not consumable");
    assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
    assert_eq!(error.path(), "$.opaque");
    seen.observe("undeclared-name-unconsumable");
    let error = decode_typed_complete::<CommonGrabResult>(
        r#"{"resultType":"complete","status":"ready","_meta":{"trace":true}}"#,
        ResultPeerEra::Modern,
    )
    .expect_err("an undeclared common member is not consumable either");
    assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
    assert_eq!(error.path(), "$._meta");
    seen.observe("undeclared-name-unconsumable");

    // PRT-04.15 — a composition that declares a common name is refused at its
    // declaration, before any input is parsed. The same rule guards encoding.
    let error = decode_typed_complete::<CommonNameClaimingResult>(
        r#"{"resultType":"complete","status":"ready"}"#,
        ResultPeerEra::Modern,
    )
    .expect_err("a composition cannot declare a common member as its own");
    assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
    assert_eq!(error.path(), "_meta");
    seen.observe("payload-declaration-collision-refused");
    for claimed_common in ["resultType", "serverInfo"] {
        let error = encode_complete_result(
            &ResultMeta::empty(),
            Vec::new(),
            &[claimed_common],
            &UnknownResultMembers::default(),
        )
        .expect_err("encoding refuses a composition that claims a common name");
        assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
        assert_eq!(error.path(), claimed_common);
        seen.observe("payload-declaration-collision-refused");
    }
    let error = encode_complete_result(
        &ResultMeta::empty(),
        Vec::new(),
        &["status", "status"],
        &UnknownResultMembers::default(),
    )
    .expect_err("a composition cannot declare the same name twice");
    assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
    seen.observe("payload-declaration-collision-refused");

    // PRT-04.16 — a typed complete decoder refuses every non-complete core
    // composition, and refuses an unclaimed discriminator outright. The typed
    // entry point injects no policy of its own, so it can never activate an
    // extension.
    let error = decode_typed_complete::<LookupResult>(
        r#"{"resultType":"input_required","requestState":"retry-1"}"#,
        ResultPeerEra::Modern,
    )
    .expect_err("input_required is not a complete composition");
    assert_eq!(error.kind(), ResultDecodeErrorKind::UnexpectedResultType);
    assert_eq!(error.path(), "$.resultType");
    seen.observe("wrong-core-composition-refused");
    let error = decode_typed_complete::<LookupResult>(
        UNCLAIMED_DISCRIMINATOR,
        ResultPeerEra::Modern,
    )
    .expect_err("typed decode cannot activate an unclaimed discriminator");
    assert_eq!(error.kind(), ResultDecodeErrorKind::RejectedExtension);
    seen.observe("wrong-core-composition-refused");

    // PRT-04.17 — locally authored extras cannot borrow the selected
    // composition's own member names, nor repeat themselves.
    for claimed_known in ["status", "record"] {
        let error = encode_complete_result(
            &ResultMeta::empty(),
            lookup_known_members(&typed.payload),
            LookupResult::KNOWN_MEMBER_NAMES,
            &UnknownResultMembers::try_new(
                vec![ExactJsonMember {
                    name: claimed_known.to_owned(),
                    value: ExactJsonValue::Bool(true),
                }],
                &[],
            )
            .expect("the extras themselves are structurally fine in isolation"),
        )
        .expect_err("an extra cannot borrow a selected-known name");
        assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
        assert_eq!(error.path(), claimed_known);
        seen.observe("local-extra-collision-refused");
    }
    let duplicate = UnknownResultMembers::try_new(
        vec![
            ExactJsonMember {
                name: "free".to_owned(),
                value: ExactJsonValue::Bool(true),
            },
            ExactJsonMember {
                name: "free".to_owned(),
                value: ExactJsonValue::Bool(false),
            },
        ],
        &[],
    )
    .expect_err("locally authored extras cannot repeat a name");
    assert_eq!(duplicate.kind(), ResultDecodeErrorKind::DuplicateMember);
    seen.observe("local-extra-collision-refused");

    // PRT-04.18 — an ordinary modern result without `_meta` and without
    // `serverInfo` is valid, reads as empty rather than missing, and explicit
    // null is still refused in the same era.
    assert!(
        no_meta.meta.metadata().is_empty(),
        "an absent _meta presents as an empty view, not an error"
    );
    seen.observe("absent-meta-and-server-info-valid");
    assert!(no_meta.meta.server_info.is_none());
    assert_eq!(
        no_meta
            .meta
            .final_server_info()
            .expect("absent identity metadata is valid"),
        None,
        "decoders accept an absent serverInfo; the final requirement is SHOULD"
    );
    seen.observe("absent-meta-and-server-info-valid");
    let error = decode_typed_complete::<LookupResult>(
        r#"{"resultType":"complete","status":"ready","record":{},"_meta":null}"#,
        ResultPeerEra::Modern,
    )
    .expect_err("explicit null _meta is not the same as an absent _meta");
    assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidKnownMember);
    assert_eq!(error.path(), "$._meta");
    seen.observe("absent-meta-and-server-info-valid");

    seen.assert_meets(&cases);
}

#[test]
fn prt_04_b_planted_negative() {
    // Named mutable state, captured before any planted input is offered.
    let cases = assert_shipped_b_manifest();
    let published_manifest_bytes = PRT_04_B_EVALUATOR_MANIFEST_V1.to_owned();
    let published_digest = prt_04_b_manifest_digest();
    let (baseline, baseline_diagnostic) =
        decode_typed_complete::<LookupResult>(TYPED_CANONICAL_COMPLETE, ResultPeerEra::Modern)
            .expect("baseline selected complete result");
    let baseline_extras = baseline.extras.clone();
    let baseline_status = baseline.payload.status.clone();
    let baseline_bytes = encode_complete_result(
        &baseline.meta,
        lookup_known_members(&baseline.payload),
        LookupResult::KNOWN_MEMBER_NAMES,
        &baseline.extras,
    )
    .expect("baseline re-encode");
    assert_eq!(baseline_bytes, TYPED_CANONICAL_COMPLETE);
    assert_eq!(baseline_diagnostic, None);

    // Planted mutation 1 — the forbidden dimension is a selected-known
    // member's JSON kind. Exactly one input dimension differs from the
    // accepted baseline; every unknown sibling is byte-identical.
    let planted_kind =
        TYPED_CANONICAL_COMPLETE.replacen(r#""status":"ready""#, r#""status":false"#, 1);
    assert_ne!(planted_kind, TYPED_CANONICAL_COMPLETE);
    let error = decode_typed_complete::<LookupResult>(&planted_kind, ResultPeerEra::Modern)
        .expect_err("only the selected status JSON kind changed");
    assert_eq!(error.kind(), ResultDecodeErrorKind::InvalidKnownMember);
    assert_eq!(error.path(), "$.status");

    // Planted mutation 2 — the forbidden dimension is the composition the
    // discriminator selects. The discriminator stays a string naming a core
    // value; only WHICH core value it names changes.
    //
    // The baseline is deliberately well formed under both compositions. An
    // earlier version of this case mutated a frame carrying neither
    // `inputRequests` nor `requestState`, so flipping the discriminator also
    // made the frame invalid for the composition it now named. It was still
    // refused — but by `MissingInputRequest` from the input-required arm,
    // which returns before the typed decoder ever reaches its composition
    // check. A refusal arriving from a different check than the one a negative
    // names proves only that *something* rejected the input, not that the
    // forbidden dimension caused the rejection.
    let accepted_composition =
        decode_typed_complete::<LookupResult>(DUAL_VALID_COMPOSITION, ResultPeerEra::Modern)
            .expect("the baseline is a valid complete composition")
            .0;
    assert_eq!(accepted_composition.payload.status, "ready");
    assert_eq!(
        extra_names(&accepted_composition.extras),
        ["requestState"],
        "read as `complete`, the input-required member is an inert sibling"
    );
    let planted_composition = DUAL_VALID_COMPOSITION.replacen(
        r#""resultType":"complete""#,
        r#""resultType":"input_required""#,
        1,
    );
    assert_ne!(planted_composition, DUAL_VALID_COMPOSITION);
    // Prove the mutated frame is a WELL-FORMED input-required result before
    // asking the typed decoder about it. This is what makes the refusal below
    // attributable to the composition mismatch and nothing else.
    let (untyped, _) = decode_peer_result(
        &planted_composition,
        ResultPeerEra::Modern,
        &CoreResultDiscriminatorPolicy,
    )
    .expect("the mutated frame is a valid input-required result, not a malformed one");
    let DecodedResult::InputRequired(untyped) = untyped else {
        panic!("the mutated discriminator selects the input-required composition");
    };
    assert_eq!(untyped.request_state(), Some("retry-1"));
    // Only now is the refusal attributable: the frame is valid, the decoder
    // simply is not the one this composition belongs to.
    let error = decode_typed_complete::<LookupResult>(&planted_composition, ResultPeerEra::Modern)
        .expect_err("a typed complete decoder must refuse a well-formed non-complete composition");
    assert_eq!(error.kind(), ResultDecodeErrorKind::UnexpectedResultType);
    assert_eq!(error.path(), "$.resultType");

    // Planted mutation 3 — the forbidden dimension is one open sibling's name,
    // changed to collide with a selected-known name. The value is untouched.
    let legal_extra = UnknownResultMembers::try_new(
        vec![ExactJsonMember {
            name: "free".to_owned(),
            value: ExactJsonValue::Bool(true),
        }],
        &[],
    )
    .expect("the accepted baseline extra");
    let accepted = encode_complete_result(
        &ResultMeta::empty(),
        lookup_known_members(&baseline.payload),
        LookupResult::KNOWN_MEMBER_NAMES,
        &legal_extra,
    )
    .expect("baseline encode with a legal extra");
    let colliding_extra = UnknownResultMembers::try_new(
        vec![ExactJsonMember {
            name: "record".to_owned(),
            value: ExactJsonValue::Bool(true),
        }],
        &[],
    )
    .expect("structurally fine until the composition is named");
    let error = encode_complete_result(
        &ResultMeta::empty(),
        lookup_known_members(&baseline.payload),
        LookupResult::KNOWN_MEMBER_NAMES,
        &colliding_extra,
    )
    .expect_err("only the extra's name changed, from `free` to `record`");
    assert_eq!(error.kind(), ResultDecodeErrorKind::KnownMemberCollision);
    assert_eq!(error.path(), "record");

    // Named mutable state, byte-for-byte unchanged across all three refusals.
    let (reaccepted, reaccepted_diagnostic) =
        decode_typed_complete::<LookupResult>(TYPED_CANONICAL_COMPLETE, ResultPeerEra::Modern)
            .expect("a refusal cannot poison a later accepted typed decode");
    assert_eq!(reaccepted.payload.status, baseline_status);
    assert_eq!(reaccepted.extras, baseline_extras);
    assert_eq!(reaccepted_diagnostic, baseline_diagnostic);
    assert_eq!(
        encode_complete_result(
            &reaccepted.meta,
            lookup_known_members(&reaccepted.payload),
            LookupResult::KNOWN_MEMBER_NAMES,
            &reaccepted.extras,
        )
        .expect("re-encode after refusal"),
        baseline_bytes
    );
    assert_eq!(
        encode_complete_result(
            &ResultMeta::empty(),
            lookup_known_members(&reaccepted.payload),
            LookupResult::KNOWN_MEMBER_NAMES,
            &legal_extra,
        )
        .expect("the accepted encode is still accepted"),
        accepted
    );
    assert_eq!(PRT_04_B_EVALUATOR_MANIFEST_V1, published_manifest_bytes);
    assert_eq!(
        prt_04_b_manifest_digest().as_bytes(),
        published_digest.as_bytes()
    );
    assert_eq!(assert_shipped_b_manifest().len(), cases.len());
}
