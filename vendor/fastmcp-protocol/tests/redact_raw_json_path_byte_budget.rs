//! Binds the character/byte equivalence inside `redact_raw_json_path`.
//!
//! `e4b70555` converted that loop to `.enumerate()` and added a comment
//! explaining why a CHARACTER counter may be compared against a BYTE budget:
//! every branch happens to push exactly one ASCII byte. The comment names the
//! edit that would break it — a Unicode replacement or an ellipsis in either
//! branch — but a comment cannot fail. These tests can.
//!
//! They drive the redactor through the shipped public surface
//! (`admit_raw_json_document` -> `RawJsonAdmissionFailure::path`) rather than a
//! `#[cfg(test)]` item, so they prove the rendering a real consumer sees.
//!
//! # Measured sensitivity
//!
//! A green test proves nothing until it has been shown to go red. Planting
//! `rendered.push('\u{2026}')` in place of `rendered.push('*')` in
//! `redact_raw_json_path` produced, on the exact three tests below:
//!
//! ```text
//! a_multibyte_member_redacts_to_one_ascii_byte_per_character ... FAILED
//! an_over_budget_multibyte_member_stays_within_the_byte_ceiling ... FAILED
//! the_allowed_character_set_survives_redaction ... ok
//! test result: FAILED. 1 passed; 2 failed                     exit=101
//! ```
//!
//! The third one PASSING is the part worth keeping. It exercises the allowed
//! character branch, which the plant did not touch, so the trio
//! DISCRIMINATES between the two branches rather than merely reacting to any
//! edit at all. A suite where all three went red would be the weaker result.
//!
//! The mutation was reverted from a pristine copy and the file's blob hash
//! verified identical to `HEAD` afterwards.

use fastmcp_protocol::{
    MAX_RAW_JSON_PATH_SEGMENT_BYTES, RawJsonTopLevel, admit_raw_json_document,
};

/// A three-byte UTF-8 character. If any redaction branch ever emits the
/// character itself, or swaps `'*'` for a multi-byte replacement, the rendered
/// byte length stops tracking the character count and these tests fail.
const MULTIBYTE: char = '☃';

/// Comfortably above any document these tests build. The scanner uses this
/// same value as its decoded-string budget, so a generous limit keeps the
/// refusal below on the value rather than on a string-length boundary.
const GENEROUS_LIMIT: usize = 64 * 1024;

/// Refuses a document whose sole member has `name`, and returns the redacted
/// path recorded at the moment of refusal.
///
/// The value `@` cannot begin a JSON value, so the scanner refuses inside the
/// member — after the member has been pushed onto the path, which is what puts
/// the redacted name into the diagnostic.
fn redacted_path_for_member(name: &str) -> String {
    let document = format!(r#"{{"{name}": @}}"#);
    let failure = admit_raw_json_document(
        document.as_bytes(),
        GENEROUS_LIMIT,
        RawJsonTopLevel::JsonRpcObject,
    )
    .expect_err("a value beginning with '@' must be refused");
    failure.path().to_owned()
}

/// The equivalence itself, pinned exactly: one multi-byte character in, one
/// single-byte `'*'` out.
#[test]
fn a_multibyte_member_redacts_to_one_ascii_byte_per_character() {
    let name: String = std::iter::repeat_n(MULTIBYTE, 8).collect();
    assert_eq!(name.len(), 24, "the fixture must really be multi-byte");

    let path = redacted_path_for_member(&name);

    assert!(
        !path.contains('~'),
        "eight characters must not reach the budget; this case is about the \
         equivalence, not truncation: {path:?}"
    );
    assert_eq!(
        path, "/********",
        "each multi-byte character must redact to exactly one '*' byte — if \
         this renders longer, a branch is emitting more than one byte and the \
         char counter no longer bounds the byte budget"
    );
}

/// The budget, exercised. A name longer than the segment budget must still
/// render within it, in bytes, not characters.
#[test]
fn an_over_budget_multibyte_member_stays_within_the_byte_ceiling() {
    let name: String =
        std::iter::repeat_n(MULTIBYTE, MAX_RAW_JSON_PATH_SEGMENT_BYTES + 16).collect();

    let path = redacted_path_for_member(&name);

    // Anti-vacuity: without this, a name that never reached the budget would
    // satisfy every assertion below while proving nothing about truncation.
    assert!(
        path.contains('~'),
        "the truncation marker must be present, or the budget was never \
         exercised and this test is vacuous: {path:?}"
    );
    assert!(
        path.is_ascii(),
        "every redacted byte must be ASCII — this is precisely what lets a \
         CHARACTER counter bound a BYTE budget: {path:?}"
    );

    // One '/' separator, at most the budget in retained bytes, one '~' marker.
    // The marker is pushed after the budget check, so it is the 65th byte.
    let ceiling = 1 + MAX_RAW_JSON_PATH_SEGMENT_BYTES + 1;
    assert!(
        path.len() <= ceiling,
        "redacted path is {} bytes against a ceiling of {ceiling}: {path:?}",
        path.len()
    );
}

/// Guards the two tests above against a redactor that stars *everything*.
///
/// Such an implementation would satisfy both of them — every output would be
/// ASCII, one byte per character, and correctly truncated — while destroying
/// the structure the diagnostic exists to carry. This is the case that makes
/// them mean something.
#[test]
fn the_allowed_character_set_survives_redaction() {
    let path = redacted_path_for_member("aZ9_.-");

    assert_eq!(
        path, "/aZ9_.-",
        "[A-Za-z0-9_.-] must pass through unredacted; a redactor that stars \
         every character would still satisfy the byte-budget tests"
    );
}
