//! Binds the byte/character invariant in `redact_raw_json_path` from OUTSIDE the
//! crate, through the shipped public admission surface.
//!
//! WHY THIS EXISTS AS A TEST AND NOT A COMMENT. `redact_raw_json_path` counts
//! CHARACTERS into `retained` and compares that count against a constant named
//! `MAX_RAW_JSON_PATH_SEGMENT_BYTES`. That is correct only because every branch
//! of its sanitiser pushes exactly one ASCII byte — an ASCII alphanumeric or
//! `_ . -` as itself, anything else as `'*'`. The equivalence is invisible from
//! the constant's name and lives three lines from the comparison.
//!
//! `jsonrpc.rs` documents that invariant in prose. Prose is not a gate: an edit
//! that replaced `'*'` with a Unicode replacement character or an ellipsis would
//! silently turn a byte budget into a character budget admitting up to four
//! times the bytes, and nothing in the suite would fail. These assertions fail.
//!
//! Two lanes independently read those twelve lines and agreed the invariant
//! held. Agreement between readers who used the same method is one confirmation,
//! not two — so this binds the invariant by execution instead of restating it.

use fastmcp_protocol::{
    MAX_RAW_JSON_PATH_SEGMENT_BYTES, RawJsonAdmissionError, RawJsonTopLevel,
    admit_raw_json_document,
};

/// A four-byte character. Chosen over a two-byte one so a regression is
/// unmistakable rather than marginal: 64 of these are 256 bytes, four times the
/// budget the constant names.
const WIDE: char = '\u{1D11E}';

/// Comfortably more characters than `MAX_RAW_JSON_PATH_SEGMENT_BYTES`, so the
/// truncation branch is reached rather than merely approached.
const WIDE_COUNT: usize = 100;

/// Refuses a document at a member whose name is entirely multi-byte, and returns
/// the redacted path the public failure carries.
fn redacted_path_for_wide_member() -> String {
    // `to_string().repeat()` rather than `repeat(..).take(..)`: this crate is
    // being cleared of clippy findings, and the iterator form can itself trip
    // `clippy::manual_repeat_n`.
    let name = WIDE.to_string().repeat(WIDE_COUNT);
    assert_eq!(
        name.chars().count(),
        WIDE_COUNT,
        "the fixture name must be {WIDE_COUNT} characters"
    );
    assert_eq!(
        name.len(),
        WIDE_COUNT * WIDE.len_utf8(),
        "the fixture name must be multi-byte, or this test proves nothing"
    );

    // A duplicated member refuses AT that member, so the failure path carries the
    // member name rather than a container step above it.
    let document = format!(r#"{{"{name}":1,"{name}":2}}"#);
    let failure = admit_raw_json_document(
        document.as_bytes(),
        64 * 1024,
        RawJsonTopLevel::SecurityDocumentObject,
    )
    .expect_err("a duplicated object member must be refused");
    assert_eq!(
        failure.error(),
        RawJsonAdmissionError::DuplicateObjectMember,
        "the fixture must fail at the member, not somewhere else"
    );
    failure.path().to_owned()
}

/// The redacted path is ASCII, whatever the document put in a member name.
///
/// This is the invariant itself rather than a consequence of it. Every character
/// the sanitiser retains is pushed as an ASCII byte; the moment any branch emits
/// a multi-byte character this fails, and it fails for the same reason the byte
/// budget would stop bounding bytes.
#[test]
fn raw_json_redacted_path_is_ascii_for_a_multibyte_member() {
    let path = redacted_path_for_wide_member();

    assert!(
        !path.is_empty(),
        "the refusal must report a member path, not the root"
    );
    assert!(
        path.is_ascii(),
        "the redacted path must be ASCII so its character count IS its byte count; \
         observed {path:?}"
    );
    assert_eq!(
        path.chars().count(),
        path.len(),
        "character count and byte count must be equal, or MAX_RAW_JSON_PATH_SEGMENT_BYTES \
         no longer bounds bytes; observed {path:?}"
    );
    assert!(
        !path.contains(WIDE),
        "no source character may survive redaction verbatim; observed {path:?}"
    );
}

/// Every segment is bounded in BYTES, which is what the constant claims.
///
/// The bound is the budget plus one, because a truncated segment carries a
/// trailing `'~'` that is deliberately outside it.
#[test]
fn raw_json_redacted_segment_is_byte_bounded_for_a_multibyte_member() {
    let path = redacted_path_for_wide_member();

    // A JSON-Pointer-style path starts with '/', so the first split piece is the
    // empty string before it.
    let segments: Vec<&str> = path.split('/').skip(1).collect();
    assert!(
        !segments.is_empty(),
        "the path must carry at least one segment; observed {path:?}"
    );

    for segment in &segments {
        assert!(
            segment.len() <= MAX_RAW_JSON_PATH_SEGMENT_BYTES + 1,
            "segment of {} bytes exceeds the {} byte budget plus its truncation marker; \
             observed {path:?}",
            segment.len(),
            MAX_RAW_JSON_PATH_SEGMENT_BYTES
        );
    }

    // The fixture is far longer than the budget, so truncation must have happened.
    // Without this the byte bound above could pass on a segment that was never
    // truncated, which would prove nothing about the branch under test.
    let truncated = segments
        .iter()
        .find(|segment| segment.ends_with('~'))
        .unwrap_or_else(|| {
            panic!("a {WIDE_COUNT}-character member must reach the truncation branch; observed {path:?}")
        });
    assert_eq!(
        truncated.len(),
        MAX_RAW_JSON_PATH_SEGMENT_BYTES + 1,
        "a truncated segment is exactly the budget plus one '~'; observed {truncated:?}"
    );
}
