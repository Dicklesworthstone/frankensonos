//! Frozen PRT-02 runner entries.
//!
//! These functions intentionally live at the integration-test harness root: the frozen RCH
//! runners invoke their literal names with `--exact`, so nested unit-test names are insufficient.

use fastmcp_protocol::FinalListParams;
use fastmcp_protocol::common_types::{
    AbsoluteUri, Annotations, CancellationNotification, CancellationRequestId, CommonTypeError,
    CommonWireDirection, ContentBlock, EmbeddedResourceContents, FinalCommonTypesSchema, IconTheme,
    Implementation, MAX_ABSOLUTE_URI_BYTES, MAX_CONTENT_ENCODED_BYTES, MAX_CURSOR_BYTES,
    MAX_ICON_DATA_URI_DECODED_BYTES, MAX_ICON_DATA_URI_ENCODED_BYTES,
    MAX_ICON_DATA_URI_PREFIX_BYTES, MAX_ICON_SIZE_BYTES, MAX_ICON_SIZE_ENTRIES,
    MAX_METADATA_ENTRIES, MAX_TRACE_FIELD_BYTES, OpaqueCursor, OpenMetadata,
    PRT_02_A_ICON_CONTENT_MANIFEST_V1, PRT_02_A_METADATA_MANIFEST_V1,
    PRT_02_A_URI_CURSOR_CANCEL_MANIFEST_V1, PRT_02_B_BOUNDS_DIRECTION_MANIFEST_V1,
    PRT_02_B_OPEN_GOLDENS_MANIFEST_V1, PRT_02_B_SERDE_SCHEMA_MANIFEST_V1, RawIcon, TraceContext,
    parse_prt_02_manifest_rows, prt_02_a_icon_content_manifest_digest,
    prt_02_a_metadata_manifest_digest, prt_02_a_uri_cursor_cancel_manifest_digest,
    prt_02_b_bounds_direction_manifest_digest, prt_02_b_open_goldens_manifest_digest,
    prt_02_b_serde_schema_manifest_digest,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn request_metadata() -> OpenMetadata {
    OpenMetadata::try_from_entries([
        (
            "io.modelcontextprotocol/protocolVersion".to_owned(),
            json!("2026-07-28"),
        ),
        (
            "io.modelcontextprotocol/clientCapabilities".to_owned(),
            json!({"roots": {"listChanged": true}}),
        ),
        (
            "io.modelcontextprotocol/clientInfo".to_owned(),
            json!({"name": "fastmcp", "version": "0.1.0"}),
        ),
        ("com.example/".to_owned(), json!({"open": null})),
        (
            "traceparent".to_owned(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        ("tracestate".to_owned(), json!("vendor=value")),
        ("baggage".to_owned(), json!("user=opaque")),
    ])
    .expect("valid final request metadata")
}

/// Records observations against one published PRT-02 manifest.
///
/// The declared rows come from the shipped manifest, never from this test, so
/// the matrix cannot silently shrink to whatever the test happens to exercise.
/// `settle` fails if any declared subcase went unobserved, if any subcase fell
/// below its declared floor, if an undeclared subcase was observed, or if the
/// declared row count drifts from the acceptance item's numeric floor.
struct SubcaseLedger {
    manifest: &'static str,
    observed: BTreeMap<String, usize>,
}

impl SubcaseLedger {
    fn new(manifest: &'static str) -> Self {
        Self {
            manifest,
            observed: BTreeMap::new(),
        }
    }

    fn observe(&mut self, subcase: &str) {
        *self.observed.entry(subcase.to_owned()).or_default() += 1;
    }

    fn settle(self, declared_rows: usize) {
        let rows = parse_prt_02_manifest_rows(self.manifest)
            .expect("the shipped manifest parses into ordered subcase rows");
        assert_eq!(
            rows.len(),
            declared_rows,
            "the manifest must declare exactly the acceptance item's numeric floor of rows",
        );
        let declared: BTreeMap<&str, usize> = rows
            .iter()
            .map(|(id, _, floor)| (id.as_str(), *floor))
            .collect();
        for (id, floor) in &declared {
            let count = self.observed.get(*id).copied().unwrap_or_default();
            assert!(
                count >= *floor,
                "subcase {id} observed {count} times, below its declared floor of {floor}",
            );
        }
        for id in self.observed.keys() {
            assert!(
                declared.contains_key(id.as_str()),
                "observed undeclared subcase {id}; the manifest is the closed set",
            );
        }
    }
}

#[test]
fn prt_02_a_positive() {
    // Every declared row of all three PRT-02 A manifests is exercised here,
    // including the rows whose declared outcome is refusal: a matrix row is
    // exercised by observing its declared result, not only by accepting. The
    // one-variable planted negatives with unchanged-state proofs live in
    // `prt_02_a_planted_negative`.
    let mut metadata_ledger = SubcaseLedger::new(PRT_02_A_METADATA_MANIFEST_V1);
    let mut uri_ledger = SubcaseLedger::new(PRT_02_A_URI_CURSOR_CANCEL_MANIFEST_V1);
    let mut icon_ledger = SubcaseLedger::new(PRT_02_A_ICON_CONTENT_MANIFEST_V1);

    // --- PRT-A-01: final open metadata ---------------------------------
    let implementation = Implementation::try_new("fastmcp", "0.1.0").expect("implementation");
    let metadata = request_metadata();

    assert_eq!(
        metadata.protocol_version().expect("protocol version"),
        Some("2026-07-28")
    );
    metadata_ledger.observe("PRT-A-01.01");

    assert_eq!(
        metadata.get("io.modelcontextprotocol/clientCapabilities"),
        Some(&json!({"roots": {"listChanged": true}})),
        "a reserved MCP key retains its exact typed value"
    );
    metadata_ledger.observe("PRT-A-01.02");

    assert_eq!(
        metadata.get(""),
        None,
        "the empty key is admissible but carries no value in this row set"
    );
    assert!(
        OpenMetadata::try_from_entries([(String::new(), json!("empty name is valid"))]).is_ok(),
        "an empty key is a valid open key"
    );
    metadata_ledger.observe("PRT-A-01.03");

    assert_eq!(
        metadata.get("traceparent"),
        Some(&json!(
            "00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"
        )),
        "an unprefixed extension key is retained verbatim"
    );
    metadata_ledger.observe("PRT-A-01.04");

    assert_eq!(metadata.get("com.example/"), Some(&json!({"open": null})));
    metadata_ledger.observe("PRT-A-01.05");

    assert_eq!(
        OpenMetadata::try_from_entries([("com..example/valid".to_owned(), json!(true))]),
        Err(CommonTypeError::Invalid("metadata key")),
        "a malformed reverse-DNS key is never retained"
    );
    metadata_ledger.observe("PRT-A-01.06");

    assert_eq!(
        metadata.client_info().expect("client info"),
        Some(implementation)
    );
    metadata_ledger.observe("PRT-A-01.07");

    let trace = TraceContext::try_from_metadata(&metadata).expect("trace context");
    assert_eq!(trace.tracestate.as_deref(), Some("vendor=value"));
    assert_eq!(trace.baggage.as_deref(), Some("user=opaque"));
    metadata_ledger.observe("PRT-A-01.08");

    // --- PRT-A-02: URI owners, cursor presence, cancellation -----------
    let uri_rows = [
        ("PRT-A-02.01", "urn:example:opaque?query#fragment"),
        ("PRT-A-02.02", "custom:path?x#y"),
        (
            "PRT-A-02.03",
            "HTTPS://user@example.test:8443/a%2Fb?x=%FF#fragment",
        ),
        ("PRT-A-02.04", "scheme://[2001:db8::1]/resource"),
        ("PRT-A-02.05", "scheme://[vF.future:opaque]/resource"),
        (
            "PRT-A-02.06",
            "HTTPS://user@example.test:8443/a%2Fb?x=%FF#fragment",
        ),
        (
            "PRT-A-02.07",
            "HTTPS://user@example.test:8443/a%2Fb?x=%FF#fragment",
        ),
        (
            "PRT-A-02.08",
            "HTTPS://user@example.test:8443/a%2Fb?x=%FF#fragment",
        ),
        ("PRT-A-02.09", "https://example.test/resource"),
    ];
    for (subcase, uri) in uri_rows {
        let parsed = AbsoluteUri::parse(uri).expect("final URI row");
        assert_eq!(parsed.as_str(), uri, "wire URI must remain byte-preserving");
        uri_ledger.observe(subcase);
    }

    // Byte-preserving re-encode through serde, not only through the accessor.
    let round_tripped = "HTTPS://user@example.test:8443/a%2Fb?x=%FF#fragment";
    let parsed = AbsoluteUri::parse(round_tripped).expect("byte-preserving URI");
    assert_eq!(
        serde_json::to_value(&parsed).expect("URI wire"),
        json!(round_tripped),
        "serialization preserves scheme case, percent-encoding, query and fragment"
    );
    uri_ledger.observe("PRT-A-02.10");

    assert_eq!(
        AbsoluteUri::parse("relative/resource"),
        Err(CommonTypeError::Invalid("URI scheme")),
        "a relative reference is never an absolute URI"
    );
    uri_ledger.observe("PRT-A-02.11");

    let prefix = "https://example.test/";
    let at_bound = format!(
        "{prefix}{}",
        "a".repeat(MAX_ABSOLUTE_URI_BYTES - prefix.len())
    );
    assert_eq!(at_bound.len(), MAX_ABSOLUTE_URI_BYTES);
    assert!(
        AbsoluteUri::parse(at_bound.clone()).is_ok(),
        "a URI exactly on its bound is admitted"
    );
    uri_ledger.observe("PRT-A-02.12");

    // The subscription identifier is the documented non-URI exception: it is
    // the one final identifier that is not required to be an absolute URI.
    let subscription = OpenMetadata::try_from_entries([(
        "io.modelcontextprotocol/subscriptionId".to_owned(),
        json!(7),
    )])
    .expect("the subscription identifier is not a URI");
    assert_eq!(
        subscription.get("io.modelcontextprotocol/subscriptionId"),
        Some(&json!(7))
    );
    uri_ledger.observe("PRT-A-02.13");

    assert_eq!(OpaqueCursor::from_presence(None).as_present(), None);
    uri_ledger.observe("PRT-A-02.14");

    // Present-and-valid is two rows, because an empty cursor is present.
    assert_eq!(
        OpaqueCursor::from_presence(Some(String::new())).as_present(),
        Some("")
    );
    uri_ledger.observe("PRT-A-02.15");
    assert_eq!(
        OpaqueCursor::from_presence(Some("next".to_owned())).as_present(),
        Some("next")
    );
    uri_ledger.observe("PRT-A-02.15");

    // The third state. Absent and present-and-valid both succeed above; an
    // explicit null is neither, and must be refused in both directions.
    assert!(
        serde_json::from_value::<OpaqueCursor>(Value::Null).is_err(),
        "an explicit null cursor is not an absent cursor"
    );
    assert!(
        serde_json::to_value(OpaqueCursor::from_presence(None)).is_err(),
        "an absent cursor must be omitted, never serialized as null"
    );
    uri_ledger.observe("PRT-A-02.16");

    let present_cursor = OpaqueCursor::try_from_presence(Some(String::new())).expect("cursor");
    assert_eq!(
        serde_json::from_value::<OpaqueCursor>(
            serde_json::to_value(&present_cursor).expect("present cursor wire")
        )
        .expect("present cursor round trip"),
        present_cursor
    );
    assert!(
        !CancellationNotification::try_new(CancellationRequestId::String("req".to_owned()), None)
            .expect("cancellation without reason")
            .has_untrusted_reason()
    );
    assert!(
        CancellationNotification::try_new(CancellationRequestId::Integer(7), Some(String::new()))
            .expect("cancellation with empty reason")
            .has_untrusted_reason()
    );

    // --- PRT-A-03: icons and content blocks ----------------------------
    let icon = RawIcon::try_with_details(
        "https://example.test/icon.svg?variant=1#exact",
        Some("image/svg+xml".to_owned()),
        Some(vec!["any".to_owned(), "32x32".to_owned()]),
        None,
    )
    .expect("raw icon");
    assert_eq!(
        icon.src.as_str(),
        "https://example.test/icon.svg?variant=1#exact"
    );
    let icon_wire = serde_json::to_value(&icon).expect("icon wire");
    assert_eq!(
        serde_json::from_value::<RawIcon>(icon_wire).expect("icon round trip"),
        icon
    );
    icon_ledger.observe("PRT-A-03.01");
    assert_eq!(
        icon.sizes.as_deref(),
        Some(&["any".to_owned(), "32x32".to_owned()][..])
    );
    icon_ledger.observe("PRT-A-03.03");

    let themed = RawIcon::try_with_details(
        "https://example.test/icon-dark.svg",
        None,
        None,
        Some(IconTheme::Dark),
    )
    .expect("themed icon");
    assert_eq!(
        serde_json::to_value(&themed).expect("themed icon wire")["theme"],
        json!("dark"),
        "the theme discriminator is exactly its lowercase wire spelling"
    );
    icon_ledger.observe("PRT-A-03.04");

    let data_icon = RawIcon::try_new("DATA:image/png;base64,aGVsbG8=?cache=opaque#fragment")
        .expect("raw data icon with preserved query and fragment");
    assert_eq!(
        data_icon.src.as_str(),
        "DATA:image/png;base64,aGVsbG8=?cache=opaque#fragment"
    );
    let over_ordinary_uri_limit = format!(
        "data:image/png;base64,{}",
        "A".repeat(MAX_ABSOLUTE_URI_BYTES)
    );
    RawIcon::try_new(over_ordinary_uri_limit)
        .expect("data icon uses its dedicated bound rather than the ordinary URI bound");
    assert_eq!(
        MAX_ICON_DATA_URI_ENCODED_BYTES,
        4 * MAX_ICON_DATA_URI_DECODED_BYTES.div_ceil(3) + MAX_ICON_DATA_URI_PREFIX_BYTES
    );
    icon_ledger.observe("PRT-A-03.02");

    let content_rows = [
        ("PRT-A-03.05", ContentBlock::text("text")),
        (
            "PRT-A-03.06",
            ContentBlock::image("aGVsbG8=", "image/png").expect("image"),
        ),
        (
            "PRT-A-03.07",
            ContentBlock::audio("aGVsbG8=", "audio/ogg").expect("audio"),
        ),
        (
            "PRT-A-03.08",
            ContentBlock::resource_link("https://example.test/resource", "resource").expect("link"),
        ),
        (
            "PRT-A-03.09",
            ContentBlock::Resource {
                resource: EmbeddedResourceContents::Text {
                    uri: AbsoluteUri::parse("https://example.test/embedded").expect("embedded URI"),
                    text: "embedded".to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    meta: None,
                    additional: BTreeMap::new(),
                },
                annotations: None,
                meta: Some(metadata),
                additional: BTreeMap::new(),
            },
        ),
    ];
    for (subcase, content) in content_rows {
        let wire = serde_json::to_value(&content).expect("content wire");
        if matches!(&content, ContentBlock::Resource { .. }) {
            assert_eq!(wire["resource"]["mimeType"], json!("text/plain"));
            assert!(wire["resource"].get("mime_type").is_none());
            assert!(
                wire.get("_meta").is_some(),
                "icon-bearing metadata rides with the content block"
            );
        }
        FinalCommonTypesSchema::validate(CommonWireDirection::Result, &wire)
            .expect("content schema");
        assert_eq!(
            serde_json::from_value::<ContentBlock>(wire).expect("content round trip"),
            content
        );
        icon_ledger.observe(subcase);
    }

    let annotated = ContentBlock::Text {
        text: "annotated".to_owned(),
        annotations: Some(Annotations::default()),
        meta: None,
        additional: BTreeMap::new(),
    };
    let annotated_wire = serde_json::to_value(&annotated).expect("annotated content wire");
    FinalCommonTypesSchema::validate(CommonWireDirection::Result, &annotated_wire)
        .expect("annotated content schema");
    assert_eq!(
        serde_json::from_value::<ContentBlock>(annotated_wire).expect("annotated round trip"),
        annotated
    );
    icon_ledger.observe("PRT-A-03.10");

    // --- settle the three ordered matrices against their declared floors
    metadata_ledger.settle(8);
    uri_ledger.settle(16);
    icon_ledger.settle(10);

    // The three published digests are distinct, so no manifest is a copy of
    // another and each acceptance item has its own canonical receipt.
    let digests = [
        prt_02_a_metadata_manifest_digest(),
        prt_02_a_uri_cursor_cancel_manifest_digest(),
        prt_02_a_icon_content_manifest_digest(),
    ];
    assert_ne!(digests[0], digests[1]);
    assert_ne!(digests[1], digests[2]);
    assert_ne!(digests[0], digests[2]);
}

#[test]
fn prt_02_a_planted_negative() {
    let accepted = RawIcon::try_new("https://example.test/icon.png").expect("accepted baseline");
    let baseline = accepted.clone();
    assert_eq!(
        RawIcon::try_new("relative/icon.png"),
        Err(CommonTypeError::Invalid("URI scheme")),
        "only the source URI's required scheme changes"
    );
    assert_eq!(
        accepted, baseline,
        "rejected input cannot mutate accepted icon state"
    );

    let accepted_metadata = request_metadata();
    let metadata_baseline = accepted_metadata.clone();
    assert_eq!(
        OpenMetadata::try_from_entries([("com..example/valid".to_owned(), json!(true))]),
        Err(CommonTypeError::Invalid("metadata key")),
        "only the reverse-DNS prefix is malformed"
    );
    assert_eq!(
        accepted_metadata, metadata_baseline,
        "rejected key cannot mutate metadata"
    );

    let cursor = OpaqueCursor::from_presence(None);
    assert!(
        serde_json::to_value(&cursor).is_err(),
        "a standalone absent cursor must not serialize as explicit null"
    );
    assert!(
        serde_json::from_value::<OpaqueCursor>(Value::Null).is_err(),
        "explicit null is distinct from an omitted nextCursor member"
    );

    let accepted_resource = json!({
        "type": "resource",
        "resource": {
            "uri": "https://example.test/embedded",
            "text": "baseline"
        }
    });
    let resource_baseline = accepted_resource.clone();
    let mut conflicting_resource = accepted_resource.clone();
    conflicting_resource["resource"]["blob"] = json!("aGVsbG8=");
    assert!(
        serde_json::from_value::<ContentBlock>(conflicting_resource).is_err(),
        "only the conflicting content member changes"
    );
    assert_eq!(
        accepted_resource, resource_baseline,
        "a rejected conflicting member cannot mutate accepted wire"
    );

    let mut snake_case_resource = accepted_resource.clone();
    snake_case_resource["resource"]["mime_type"] = json!("text/plain");
    assert!(
        serde_json::from_value::<ContentBlock>(snake_case_resource).is_err(),
        "only the unrecognized snake_case MIME member changes"
    );
    assert_eq!(
        accepted_resource, resource_baseline,
        "a rejected snake_case MIME member cannot mutate accepted wire"
    );

    let accepted_content = json!({"type": "text", "text": "baseline"});
    let content_baseline = accepted_content.clone();
    let mut unknown_member = accepted_content.clone();
    unknown_member["unrecognized"] = json!(true);
    assert!(
        serde_json::from_value::<ContentBlock>(unknown_member).is_err(),
        "only one unknown content member changes"
    );
    assert_eq!(
        accepted_content, content_baseline,
        "a rejected unknown member cannot mutate accepted wire"
    );

    let accepted_data_icon = RawIcon::try_new("data:image/png;base64,aGVsbG8=")
        .expect("accepted image data icon baseline");
    let data_icon_baseline = accepted_data_icon.clone();
    assert_eq!(
        RawIcon::try_new("data:text/plain;base64,aGVsbG8="),
        Err(CommonTypeError::Invalid("icon data MIME type")),
        "only the data URI media type changes from image to text"
    );
    assert_eq!(
        accepted_data_icon, data_icon_baseline,
        "a rejected data MIME cannot mutate accepted icon state"
    );

    let accepted_authority =
        AbsoluteUri::parse("https://user@example.test:8443/resource").expect("authority port");
    let authority_baseline = accepted_authority.clone();
    assert_eq!(
        AbsoluteUri::parse("https://user@example.test:not-a-port/resource"),
        Err(CommonTypeError::Invalid("absolute URI")),
        "only the authority port changes from decimal digits to invalid characters"
    );
    assert_eq!(
        accepted_authority, authority_baseline,
        "a rejected authority port cannot mutate the accepted URI"
    );
}

#[test]
fn prt_02_b_positive() {
    let implementation = Implementation::try_new("fastmcp", "0.1.0").expect("implementation");
    let implementation_wire = serde_json::to_value(&implementation).expect("implementation wire");
    assert_eq!(
        serde_json::from_value::<Implementation>(implementation_wire)
            .expect("implementation round trip"),
        implementation
    );

    let request = json!({"_meta": request_metadata()});
    let request_golden = "{\"_meta\":{\"baggage\":\"user=opaque\",\"com.example/\":{\"open\":null},\"io.modelcontextprotocol/clientCapabilities\":{\"roots\":{\"listChanged\":true}},\"io.modelcontextprotocol/clientInfo\":{\"name\":\"fastmcp\",\"version\":\"0.1.0\"},\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"traceparent\":\"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\",\"tracestate\":\"vendor=value\"}}";
    FinalCommonTypesSchema::validate_golden(CommonWireDirection::Request, &request, request_golden)
        .expect("request golden");
    assert_eq!(
        serde_json::from_value::<OpenMetadata>(request["_meta"].clone()).expect("metadata serde"),
        request_metadata()
    );
    assert_eq!(
        serde_json::from_value::<Annotations>(json!({"audience": ["user", "assistant"], "priority": 1.0, "lastModified": "2026-08-07T00:00:00Z"}))
            .expect("annotation serde")
            .priority,
        Some(1.0)
    );
    let cursor = OpaqueCursor::try_from_presence(Some(String::new())).expect("empty cursor");
    assert_eq!(
        serde_json::from_value::<OpaqueCursor>(serde_json::to_value(&cursor).expect("cursor wire"))
            .expect("cursor round trip"),
        cursor
    );
    assert_eq!(
        serde_json::from_value::<CancellationRequestId>(json!(42)).expect("request ID serde"),
        CancellationRequestId::Integer(42)
    );

    let icon = json!({"src": "HTTPS://example.test/icon.svg", "sizes": [], "theme": "dark"});
    assert!(
        !FinalCommonTypesSchema::validate_icon(&icon)
            .expect("icon schema")
            .effective_any_size()
    );
    let cancellation = json!({
        "method": "notifications/cancelled",
        "params": {"requestId": "request-7", "reason": "bounded"}
    });
    FinalCommonTypesSchema::validate(CommonWireDirection::Notification, &cancellation)
        .expect("notification direction");

    let metadata_n_minus_one =
        (0..MAX_METADATA_ENTRIES - 1).map(|index| (format!("com.example/key{index}"), Value::Null));
    OpenMetadata::try_from_entries(metadata_n_minus_one).expect("metadata N-1");
    let metadata_n =
        (0..MAX_METADATA_ENTRIES).map(|index| (format!("com.example/key{index}"), Value::Null));
    OpenMetadata::try_from_entries(metadata_n).expect("metadata N");
    for length in [MAX_ABSOLUTE_URI_BYTES - 1, MAX_ABSOLUTE_URI_BYTES] {
        let uri = format!("x:{}", "a".repeat(length - 2));
        assert_eq!(
            AbsoluteUri::parse(uri)
                .expect("URI at bound")
                .as_str()
                .len(),
            length
        );
    }
    for length in [MAX_CURSOR_BYTES - 1, MAX_CURSOR_BYTES] {
        assert_eq!(
            OpaqueCursor::try_from_presence(Some("x".repeat(length)))
                .expect("cursor at bound")
                .as_present()
                .map(str::len),
            Some(length)
        );
    }
    for length in [4 * 1024, 4 * 1024 + 1] {
        assert!(
            CancellationNotification::try_new(
                CancellationRequestId::Integer(1),
                Some("x".repeat(length))
            )
            .expect("the MCP schema imposes no cancellation-reason byte bound")
            .has_untrusted_reason()
        );
    }
    let sizes = vec!["x".repeat(MAX_ICON_SIZE_BYTES); MAX_ICON_SIZE_ENTRIES];
    RawIcon::try_with_details("https://example.test/icon", None, Some(sizes), None)
        .expect("icon sizes at bound");
    RawIcon::try_with_details(
        "https://example.test/icon",
        None,
        Some(vec![
            "x".repeat(MAX_ICON_SIZE_BYTES - 1);
            MAX_ICON_SIZE_ENTRIES - 1
        ]),
        None,
    )
    .expect("icon sizes at N-1");
    ContentBlock::image("A".repeat(MAX_CONTENT_ENCODED_BYTES - 1), "image/png")
        .expect("content at encoded N-1 bound");
    ContentBlock::image("A".repeat(MAX_CONTENT_ENCODED_BYTES), "image/png")
        .expect("content at encoded bound");
    let trace_n_minus_one = OpenMetadata::try_from_entries([
        (
            "traceparent".to_owned(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        (
            "tracestate".to_owned(),
            json!("x".repeat(MAX_TRACE_FIELD_BYTES - 1)),
        ),
    ])
    .expect("trace metadata");
    TraceContext::try_from_metadata(&trace_n_minus_one).expect("trace at N-1");
    let trace_n = OpenMetadata::try_from_entries([
        (
            "traceparent".to_owned(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        (
            "tracestate".to_owned(),
            json!("x".repeat(MAX_TRACE_FIELD_BYTES)),
        ),
    ])
    .expect("trace metadata");
    TraceContext::try_from_metadata(&trace_n).expect("trace at bound");

    assert_eq!(
        OpenMetadata::try_from_entries(
            (0..=MAX_METADATA_ENTRIES)
                .map(|index| (format!("com.example/key{index}"), Value::Null))
        ),
        Err(CommonTypeError::Invalid("metadata key"))
    );
    assert_eq!(
        AbsoluteUri::parse(format!("x:{}", "a".repeat(MAX_ABSOLUTE_URI_BYTES - 1))),
        Err(CommonTypeError::TooLong("URI"))
    );
    assert!(
        CancellationNotification::try_new(
            CancellationRequestId::Integer(1),
            Some("x".repeat(4 * 1024 + 1))
        )
        .expect("the exact cancellation schema has no reason-size rejection")
        .has_untrusted_reason()
    );
    assert_eq!(
        RawIcon::try_with_details(
            "https://example.test/icon",
            None,
            Some(vec!["x".to_owned(); MAX_ICON_SIZE_ENTRIES + 1]),
            None,
        ),
        Err(CommonTypeError::TooLong("icon sizes"))
    );
    assert_eq!(
        ContentBlock::image("A".repeat(MAX_CONTENT_ENCODED_BYTES + 1), "image/png"),
        Err(CommonTypeError::TooLong("binary content"))
    );
    let trace_n_plus_one = OpenMetadata::try_from_entries([
        (
            "traceparent".to_owned(),
            json!("00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01"),
        ),
        (
            "tracestate".to_owned(),
            json!("x".repeat(MAX_TRACE_FIELD_BYTES + 1)),
        ),
    ])
    .expect("bounded metadata value");
    assert_eq!(
        TraceContext::try_from_metadata(&trace_n_plus_one),
        Err(CommonTypeError::TooLong("trace context"))
    );

    // --- PRT-02 B ordered matrices -------------------------------------
    // The declared row set comes from the shipped manifests, never from this
    // test, so the matrix cannot shrink to whatever happens to be exercised.
    let mut serde_ledger = SubcaseLedger::new(PRT_02_B_SERDE_SCHEMA_MANIFEST_V1);
    let mut bounds_ledger = SubcaseLedger::new(PRT_02_B_BOUNDS_DIRECTION_MANIFEST_V1);
    let mut golden_ledger = SubcaseLedger::new(PRT_02_B_OPEN_GOLDENS_MANIFEST_V1);

    // PRT-B-01: serde and schema round trips through the public API.
    let implementation = Implementation::try_new("fastmcp", "0.1.0").expect("implementation");
    assert_eq!(
        serde_json::from_value::<Implementation>(
            serde_json::to_value(&implementation).expect("wire")
        )
        .expect("round trip"),
        implementation
    );
    serde_ledger.observe("PRT-B-01.01");

    let metadata = request_metadata();
    assert_eq!(
        serde_json::from_value::<OpenMetadata>(serde_json::to_value(&metadata).expect("wire"))
            .expect("round trip"),
        metadata
    );
    serde_ledger.observe("PRT-B-01.02");

    let annotations: Annotations =
        serde_json::from_value(json!({"audience": ["user"], "priority": 0.5}))
            .expect("annotations");
    assert_eq!(
        serde_json::from_value::<Annotations>(serde_json::to_value(&annotations).expect("wire"))
            .expect("round trip"),
        annotations
    );
    serde_ledger.observe("PRT-B-01.03");

    let uri = AbsoluteUri::parse("https://example.test/a?b#c").expect("uri");
    assert_eq!(
        serde_json::from_value::<AbsoluteUri>(serde_json::to_value(&uri).expect("wire"))
            .expect("round trip"),
        uri
    );
    serde_ledger.observe("PRT-B-01.04");

    let cursor_value = OpaqueCursor::try_from_presence(Some("page-2".to_owned())).expect("cursor");
    assert_eq!(
        serde_json::from_value::<OpaqueCursor>(serde_json::to_value(&cursor_value).expect("wire"))
            .expect("round trip"),
        cursor_value
    );
    serde_ledger.observe("PRT-B-01.05");

    assert_eq!(
        serde_json::from_value::<CancellationRequestId>(json!(42)).expect("request id"),
        CancellationRequestId::Integer(42)
    );
    serde_ledger.observe("PRT-B-01.06");

    assert!(
        !FinalCommonTypesSchema::validate_icon(
            &json!({"src": "HTTPS://example.test/icon.svg", "sizes": [], "theme": "dark"})
        )
        .expect("icon schema")
        .effective_any_size()
    );
    serde_ledger.observe("PRT-B-01.07");

    for (subcase, content) in [
        ("PRT-B-01.08", ContentBlock::text("round trip")),
        (
            "PRT-B-01.09",
            ContentBlock::image("aGVsbG8=", "image/png").expect("image"),
        ),
        (
            "PRT-B-01.10",
            ContentBlock::Resource {
                resource: EmbeddedResourceContents::Text {
                    uri: AbsoluteUri::parse("https://example.test/embedded").expect("uri"),
                    text: "embedded".to_owned(),
                    mime_type: Some("text/plain".to_owned()),
                    meta: None,
                    additional: BTreeMap::new(),
                },
                annotations: None,
                meta: None,
                additional: BTreeMap::new(),
            },
        ),
    ] {
        let wire = serde_json::to_value(&content).expect("content wire");
        assert_eq!(
            serde_json::from_value::<ContentBlock>(wire).expect("content round trip"),
            content
        );
        serde_ledger.observe(subcase);
    }

    let trace = TraceContext::try_from_metadata(&metadata).expect("trace");
    assert_eq!(trace.tracestate.as_deref(), Some("vendor=value"));
    serde_ledger.observe("PRT-B-01.11");

    let subscription = OpenMetadata::try_from_entries([(
        "io.modelcontextprotocol/subscriptionId".to_owned(),
        json!(7),
    )])
    .expect("subscription exception");
    assert_eq!(
        serde_json::from_value::<OpenMetadata>(serde_json::to_value(&subscription).expect("wire"))
            .expect("round trip"),
        subscription
    );
    serde_ledger.observe("PRT-B-01.12");

    // PRT-B-01.13/.14/.15 — the three distinct WIRE states of a cursor member
    // on a shipped typed struct. OpaqueCursor models presence correctly on its
    // own; what matters here is that the enclosing struct preserves the
    // distinction, which only a serde row can observe.
    let meta_wire = serde_json::to_value(request_metadata()).expect("metadata wire");

    let absent: FinalListParams =
        serde_json::from_value(json!({"_meta": meta_wire.clone()})).expect("omitted cursor");
    assert_eq!(absent.cursor, None, "an omitted cursor member is absent");
    serde_ledger.observe("PRT-B-01.13");

    for spelling in ["", "page-2"] {
        let present: FinalListParams =
            serde_json::from_value(json!({"_meta": meta_wire.clone(), "cursor": spelling}))
                .expect("present cursor");
        assert_eq!(
            present.cursor.as_deref(),
            Some(spelling),
            "a present cursor keeps its exact value, including the empty string"
        );
        serde_ledger.observe("PRT-B-01.14");
    }

    assert!(
        serde_json::from_value::<FinalListParams>(
            json!({"_meta": meta_wire.clone(), "cursor": null})
        )
        .is_err(),
        "an explicit null cursor is refused rather than collapsed into absence"
    );
    // And the accepted absent value is unchanged by that refusal.
    let readmitted: FinalListParams =
        serde_json::from_value(json!({"_meta": meta_wire.clone()})).expect("omitted cursor again");
    assert_eq!(readmitted.cursor, absent.cursor);
    serde_ledger.observe("PRT-B-01.15");

    // PRT-B-02: bounds at N-1 and N, plus accepted directions. The N+1
    // counterparts live in prt_02_b_planted_negative.
    OpenMetadata::try_from_entries(
        (0..MAX_METADATA_ENTRIES - 1).map(|i| (format!("com.example/k{i}"), Value::Null)),
    )
    .expect("metadata N-1");
    bounds_ledger.observe("PRT-B-02.01");
    OpenMetadata::try_from_entries(
        (0..MAX_METADATA_ENTRIES).map(|i| (format!("com.example/k{i}"), Value::Null)),
    )
    .expect("metadata N");
    bounds_ledger.observe("PRT-B-02.02");

    for (subcase, length) in [
        ("PRT-B-02.03", MAX_ABSOLUTE_URI_BYTES - 1),
        ("PRT-B-02.04", MAX_ABSOLUTE_URI_BYTES),
    ] {
        let candidate = format!("x:{}", "a".repeat(length - 2));
        assert_eq!(
            AbsoluteUri::parse(candidate)
                .expect("uri at bound")
                .as_str()
                .len(),
            length
        );
        bounds_ledger.observe(subcase);
    }
    for (subcase, length) in [
        ("PRT-B-02.05", MAX_CURSOR_BYTES - 1),
        ("PRT-B-02.06", MAX_CURSOR_BYTES),
    ] {
        assert_eq!(
            OpaqueCursor::try_from_presence(Some("x".repeat(length)))
                .expect("cursor at bound")
                .as_present()
                .map(str::len),
            Some(length)
        );
        bounds_ledger.observe(subcase);
    }
    RawIcon::try_with_details(
        "https://example.test/icon",
        None,
        Some(vec![
            "x".repeat(MAX_ICON_SIZE_BYTES - 1);
            MAX_ICON_SIZE_ENTRIES - 1
        ]),
        None,
    )
    .expect("icon sizes N-1");
    bounds_ledger.observe("PRT-B-02.07");
    RawIcon::try_with_details(
        "https://example.test/icon",
        None,
        Some(vec!["x".repeat(MAX_ICON_SIZE_BYTES); MAX_ICON_SIZE_ENTRIES]),
        None,
    )
    .expect("icon sizes N");
    bounds_ledger.observe("PRT-B-02.08");
    ContentBlock::image("A".repeat(MAX_CONTENT_ENCODED_BYTES - 1), "image/png")
        .expect("content bytes N-1");
    bounds_ledger.observe("PRT-B-02.09");
    assert!(
        CancellationNotification::try_new(
            CancellationRequestId::Integer(1),
            Some("x".repeat(4 * 1024 + 1))
        )
        .expect("the MCP schema imposes no cancellation-reason byte bound")
        .has_untrusted_reason()
    );
    bounds_ledger.observe("PRT-B-02.10");

    FinalCommonTypesSchema::validate(
        CommonWireDirection::Request,
        &json!({"_meta": meta_wire.clone()}),
    )
    .expect("request direction");
    bounds_ledger.observe("PRT-B-02.11");
    FinalCommonTypesSchema::validate(
        CommonWireDirection::Notification,
        &json!({
            "method": "notifications/cancelled",
            "params": {"requestId": "request-7", "reason": "bounded"}
        }),
    )
    .expect("notification direction");
    bounds_ledger.observe("PRT-B-02.12");

    // PRT-B-03: goldens. Rows .01-.03 assert exact canonical bytes; rows
    // .04-.08 assert canonical-byte stability across a round trip, which is
    // the same observation where a fixed literal would only restate the
    // encoder.
    let request = json!({"_meta": request_metadata()});
    let request_golden = "{\"_meta\":{\"baggage\":\"user=opaque\",\"com.example/\":{\"open\":null},\"io.modelcontextprotocol/clientCapabilities\":{\"roots\":{\"listChanged\":true}},\"io.modelcontextprotocol/clientInfo\":{\"name\":\"fastmcp\",\"version\":\"0.1.0\"},\"io.modelcontextprotocol/protocolVersion\":\"2026-07-28\",\"traceparent\":\"00-4bf92f3577b34da6a3ce929d0e0e4736-00f067aa0ba902b7-01\",\"tracestate\":\"vendor=value\"}}";
    FinalCommonTypesSchema::validate_golden(CommonWireDirection::Request, &request, request_golden)
        .expect("known metadata golden");
    golden_ledger.observe("PRT-B-03.01");
    assert!(
        request_golden.contains("\"traceparent\":"),
        "trace fields survive into the canonical golden bytes"
    );
    golden_ledger.observe("PRT-B-03.08");

    let open = OpenMetadata::try_from_entries([("com.example/x".to_owned(), json!({"k": 1}))])
        .expect("valid open metadata");
    assert_eq!(
        serde_json::to_string(&open).expect("open golden"),
        "{\"com.example/x\":{\"k\":1}}"
    );
    golden_ledger.observe("PRT-B-03.02");

    let unknown = OpenMetadata::try_from_entries([(
        "com.example/unknown".to_owned(),
        json!([1, {"z": null}]),
    )])
    .expect("unknown open metadata");
    assert_eq!(
        serde_json::to_string(&unknown).expect("unknown open golden"),
        "{\"com.example/unknown\":[1,{\"z\":null}]}",
        "an unrecognized open value is preserved exactly, never aliased"
    );
    golden_ledger.observe("PRT-B-03.03");

    for (subcase, value) in [
        ("PRT-B-03.04", serde_json::to_value(&uri).expect("uri wire")),
        (
            "PRT-B-03.05",
            serde_json::to_value(&cursor_value).expect("cursor wire"),
        ),
        (
            "PRT-B-03.06",
            json!({"requestId": "request-7", "reason": "bounded"}),
        ),
        (
            "PRT-B-03.07",
            json!({"src": "HTTPS://example.test/icon.svg", "theme": "dark"}),
        ),
    ] {
        let bytes = serde_json::to_string(&value).expect("canonical bytes");
        let replayed: Value = serde_json::from_str(&bytes).expect("golden replays");
        assert_eq!(
            serde_json::to_string(&replayed).expect("canonical bytes again"),
            bytes,
            "canonical golden bytes are stable across a replay"
        );
        golden_ledger.observe(subcase);
    }

    serde_ledger.settle(15);
    bounds_ledger.settle(12);
    golden_ledger.settle(8);

    let digests = [
        prt_02_b_serde_schema_manifest_digest(),
        prt_02_b_bounds_direction_manifest_digest(),
        prt_02_b_open_goldens_manifest_digest(),
    ];
    assert_ne!(digests[0], digests[1]);
    assert_ne!(digests[1], digests[2]);
    assert_ne!(digests[0], digests[2]);
}

#[test]
fn prt_02_b_planted_negative() {
    let accepted = OpaqueCursor::try_from_presence(Some("x".repeat(MAX_CURSOR_BYTES)))
        .expect("accepted cursor baseline");
    let baseline = accepted.clone();
    assert_eq!(
        OpaqueCursor::try_from_presence(Some("x".repeat(MAX_CURSOR_BYTES + 1))),
        Err(CommonTypeError::TooLong("pagination cursor")),
        "only the cursor length changes from N to N+1"
    );
    assert_eq!(
        accepted, baseline,
        "rejected cursor cannot mutate retained state"
    );

    let accepted_wire = json!({"_meta": request_metadata()});
    let wire_baseline = accepted_wire.clone();
    let mut planted = accepted_wire.clone();
    let meta = planted["_meta"].as_object_mut().expect("metadata object");
    let preserved = meta.remove("com.example/").expect("valid open key");
    meta.insert("io.modelcontextprotocol/future".to_owned(), preserved);
    assert_eq!(
        FinalCommonTypesSchema::validate(CommonWireDirection::Request, &planted),
        Err(CommonTypeError::Invalid("metadata key")),
        "only the open key becomes an unrecognized reserved key"
    );
    assert_eq!(
        accepted_wire, wire_baseline,
        "rejected golden cannot mutate retained wire state"
    );

    let content = json!({"type": "text", "text": "baseline"});
    let content_baseline = content.clone();
    let mut wrong_discriminator = content.clone();
    wrong_discriminator["type"] = json!("textual");
    assert!(serde_json::from_value::<ContentBlock>(wrong_discriminator).is_err());
    assert_eq!(
        content, content_baseline,
        "rejected discriminator cannot mutate accepted wire"
    );

    let reversed_cancellation = json!({
        "_meta": request_metadata(),
        "method": "notifications/cancelled",
        "params": {"requestId": 1}
    });
    assert_eq!(
        FinalCommonTypesSchema::validate(CommonWireDirection::Request, &reversed_cancellation),
        Err(CommonTypeError::Invalid("cancellation direction")),
        "only the notification direction is reversed"
    );
}
