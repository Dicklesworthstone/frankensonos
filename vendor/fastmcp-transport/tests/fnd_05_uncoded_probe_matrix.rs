//! FND-05 si9y: the uncoded request content-coding probe matrix, from OUTSIDE.
//!
//! The package body requires a request/response probe matrix over
//! `Content-Encoding`: absent, case-varied singleton `identity`, and bounded
//! empty-element variants as POSITIVES; all-empty, empty-element N+1,
//! parameterized, duplicate-field, multi-semantic-coding and other-coding as
//! NEGATIVES.
//!
//! SURFACE. Value-level cases drive
//! `HttpRequestHandler::admit_modern_request`, the shipped public admission
//! entry point — every type used here is re-exported from the crate root. The
//! duplicate-FIELD-LINE case cannot be expressed there at all, because
//! `HttpRequest::headers` is a `HashMap<String, String>` and therefore cannot
//! hold two `Content-Encoding` lines; that case belongs to the wire parser and
//! is driven through `HttpTransport::read_request` below.
//!
//! RH-5 DISCIPLINE. Every case is built by the same constructor and differs in
//! exactly one dimension: the `Content-Encoding` value. Method, path,
//! content-type, protocol-version mirror and body are byte-identical across
//! the whole matrix, so an outcome difference can only be attributable to the
//! coding value.
//!
//! WHAT IS NOT COVERED HERE. The RESPONSE half of the matrix (omitted response
//! `Content-Encoding`, auto-decompression disabled) requires a completed
//! `GuardedHttpFetcher` fetch response, and both routes to eliciting one are
//! `#[cfg(test)]` — see the exchange-seam decision on bd-ho7of (comment 2559).
//! It is recorded there as unprovable from the public surface at this revision
//! rather than silently omitted.

#![forbid(unsafe_code)]

use std::io::Cursor;

use fastmcp_transport::http::{
    HttpError, HttpMethod, HttpRequest, HttpRequestHandler, HttpTransport,
};

/// One valid JSON-RPC request body, identical for every case in the matrix.
fn body() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/list",
        "params": {
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            }
        },
        "id": 73
    }))
    .expect("the fixed JSON-RPC body serializes")
}

/// Builds the identical admitted request, varying ONLY `Content-Encoding`.
fn request_with_coding(handler: &HttpRequestHandler, coding: Option<&str>) -> HttpRequest {
    let request = HttpRequest::new(HttpMethod::Post, handler.config().base_path.clone())
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json")
        .with_header("MCP-Protocol-Version", "2026-07-28")
        .with_header("Mcp-Method", "tools/list");
    let request = match coding {
        Some(value) => request.with_header("content-encoding", value),
        None => request,
    };
    request.with_body(body())
}

/// The admission outcome for one coding value, rendered for diagnosis.
///
/// Returns the outcome as a short string rather than a bool. A bool collapses
/// three distinct results — admitted, refused FOR THE CODING, and refused for
/// some unrelated reason — into two, and a negative that reports "admitted"
/// when the request was actually refused for a different reason sends the
/// reader hunting the wrong defect. This is the same conflated-assertion flaw
/// that made `hostname refusal must precede HTTP bytes` unreadable from its
/// left/right values alone.
fn admission_outcome(coding: Option<&str>) -> String {
    let handler = HttpRequestHandler::new();
    match handler.admit_modern_request(&request_with_coding(&handler, coding)) {
        Ok(_) => "ADMITTED".to_owned(),
        Err(HttpError::UnsupportedContentEncoding(value)) => {
            format!("refused-for-coding({value:?})")
        }
        Err(other) => format!("refused-for-OTHER-reason({other:?})"),
    }
}

/// True when admission refused this request specifically for its content coding.
fn refused_for_coding(coding: Option<&str>) -> bool {
    admission_outcome(coding).starts_with("refused-for-coding")
}

/// `n` empty list elements followed by one `identity` token.
fn empty_elements_then_identity(n: usize) -> String {
    format!("{}identity", ",".repeat(n))
}

/// Which admission guard is expected to refuse a given value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Guard {
    /// Refused by content-coding list semantics.
    Coding,
    /// Refused earlier, by header-value validation, before list syntax applies.
    HeaderValue,
}

impl Guard {
    fn matches(self, outcome: &str) -> bool {
        match self {
            Self::Coding => outcome.starts_with("refused-for-coding"),
            Self::HeaderValue => outcome.contains("InvalidHeader"),
        }
    }
}

/// POSITIVES: absent, case-varied singleton `identity`, and bounded
/// empty-element variants in leading, interior and trailing position.
#[test]
fn fnd_05_uncoded_request_matrix_positives() {
    // The bound is 16 ignored empty elements; 16 is admitted, 17 is not.
    let sixteen = empty_elements_then_identity(16);
    let cases: Vec<(String, &str)> = vec![
        ("identity".to_owned(), "canonical lowercase singleton"),
        ("IDENTITY".to_owned(), "uppercase singleton"),
        ("Identity".to_owned(), "mixed-case singleton"),
        ("iDeNtItY".to_owned(), "alternating-case singleton"),
        (" identity ".to_owned(), "surrounding optional whitespace"),
        (",identity".to_owned(), "one leading empty element"),
        ("identity,".to_owned(), "one trailing empty element"),
        (",,identity".to_owned(), "two leading empty elements"),
        ("identity,,".to_owned(), "two trailing empty elements"),
        (", ,identity".to_owned(), "empty elements with whitespace"),
        (sixteen, "empty-element bound N = 16, admitted"),
    ];

    let mut refused = Vec::new();
    for (value, label) in &cases {
        if refused_for_coding(Some(value)) {
            refused.push(format!("{label}: Content-Encoding: {value:?} was REFUSED"));
        }
    }

    // Absent header is the baseline positive: an uncoded request carries no
    // Content-Encoding at all.
    let handler = HttpRequestHandler::new();
    let absent = handler.admit_modern_request(&request_with_coding(&handler, None));
    assert!(
        absent.is_ok(),
        "a request with NO Content-Encoding must be admitted; if this fails the whole matrix \
         below is measuring something other than content coding. Got {absent:?}"
    );

    assert!(
        refused.is_empty(),
        "{} of {} uncoded-equivalent request(s) were refused. RFC 9110 list syntax admits \
         bounded empty elements and the token is case-insensitive, so each line is an \
         over-refusal:\n{}",
        refused.len(),
        cases.len(),
        refused.join("\n"),
    );
}

/// NEGATIVES: all-empty, empty-element N+1, parameterized, multi-semantic
/// coding, and other codings. NONE may be admitted.
///
/// This is the discriminator for the positives above. A handler that admitted
/// everything would satisfy the positive test perfectly; these cases are what
/// separate "admits uncoded requests" from "admits anything".
///
/// WHICH GUARD CATCHES EACH IS PART OF THE ASSERTION. Thirteen of these are
/// content-coding violations and must reach the coding refusal. The all-empty
/// value is caught EARLIER, by header-value validation, and never reaches
/// coding semantics at all — a present field with an empty value is malformed
/// before its list syntax is considered.
///
/// The first version of this test demanded `UnsupportedContentEncoding` for
/// all fourteen and reported the all-empty case as "was ADMITTED" when it had
/// in fact been refused by that earlier guard. That message was wrong in the
/// most costly direction: it named a security-relevant admission that was not
/// happening. Asserting the GUARD as well as the refusal is what makes the
/// difference legible instead of alarming.
#[test]
fn fnd_05_uncoded_request_matrix_planted_negatives() {
    // (value, label, expected guard) — Coding for list-syntax violations,
    // HeaderValue for values malformed before list syntax applies.
    let seventeen = empty_elements_then_identity(17);
    let cases: Vec<(String, &str, Guard)> = vec![
        (String::new(), "all-empty value", Guard::HeaderValue),
        (",".to_owned(), "all-empty, two elements", Guard::Coding),
        (",,,".to_owned(), "all-empty, four elements", Guard::Coding),
        (seventeen, "empty-element bound N+1 = 17", Guard::Coding),
        (
            "identity;q=1".to_owned(),
            "parameterized token",
            Guard::Coding,
        ),
        (
            "identity ;q=1".to_owned(),
            "parameterized with whitespace",
            Guard::Coding,
        ),
        ("gzip".to_owned(), "other coding", Guard::Coding),
        ("br".to_owned(), "other coding", Guard::Coding),
        ("deflate".to_owned(), "other coding", Guard::Coding),
        (
            "identity, gzip".to_owned(),
            "two semantic codings",
            Guard::Coding,
        ),
        (
            "gzip, identity".to_owned(),
            "two semantic codings, reordered",
            Guard::Coding,
        ),
        (
            "identity, identity".to_owned(),
            "duplicate semantic token",
            Guard::Coding,
        ),
        ("x-identity".to_owned(), "near-miss token", Guard::Coding),
        (
            "identityy".to_owned(),
            "near-miss, one byte longer",
            Guard::Coding,
        ),
    ];

    let mut admitted = Vec::new();
    let mut wrong_guard = Vec::new();
    for (value, label, expected) in &cases {
        let outcome = admission_outcome(Some(value));
        if outcome == "ADMITTED" {
            admitted.push(format!("{label}: Content-Encoding: {value:?} was ADMITTED"));
        } else if !expected.matches(&outcome) {
            wrong_guard.push(format!(
                "{label}: Content-Encoding: {value:?} expected {expected:?}, got {outcome}"
            ));
        }
    }

    // THE SECURITY PROPERTY: nothing coded or malformed is admitted.
    assert!(
        admitted.is_empty(),
        "{} of {} coded or malformed request(s) were ADMITTED. A present Content-Encoding must \
         reduce to exactly one semantic `identity` token; anything else must fail before body \
         processing:\n{}",
        admitted.len(),
        cases.len(),
        admitted.join("\n"),
    );

    // THE ATTRIBUTION PROPERTY: each refusal comes from the guard that should
    // own it. A case migrating between guards is a real change in where the
    // boundary sits, and it should be noticed deliberately rather than absorbed.
    assert!(
        wrong_guard.is_empty(),
        "{} case(s) were refused by a different guard than expected. The request was still \
         refused, so this is NOT an admission defect — it means the boundary moved:\n{}",
        wrong_guard.len(),
        wrong_guard.join("\n"),
    );

    // THE ONE-VARIABLE CONTRAST, asserted rather than implicit: the bound is
    // exactly 16, and the two values differ by a single comma.
    assert!(
        !refused_for_coding(Some(&empty_elements_then_identity(16))),
        "16 empty elements must be admitted"
    );
    assert!(
        refused_for_coding(Some(&empty_elements_then_identity(17))),
        "17 empty elements must be refused; if 16 and 17 behave alike the bound is not enforced \
         at the declared value"
    );
}

/// DUPLICATE FIELD LINES, which the admission surface cannot express.
///
/// `HttpRequest::headers` is a `HashMap`, so two `Content-Encoding` lines
/// cannot survive to admission — the wire parser must refuse them first. This
/// drives the raw reader directly so the case is covered on the surface that
/// actually decides it.
#[test]
fn fnd_05_uncoded_duplicate_field_line_is_refused() {
    const DUPLICATE: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\n\
         Content-Length: 0\r\nContent-Encoding: identity\r\nContent-Encoding: gzip\r\n\r\n";

    let mut transport = HttpTransport::new(Cursor::new(DUPLICATE.as_bytes().to_vec()), Vec::new());
    let outcome = transport.read_request();
    assert!(
        outcome.is_err(),
        "two Content-Encoding field lines must be refused by the wire parser; admitting them \
         lets a HashMap silently drop one and decide framing from whichever survived. \
         Got {outcome:?}"
    );

    // Discriminator: the SAME request with one field line is accepted, so the
    // refusal tracks duplication rather than the header's presence.
    const SINGLE: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\n\
         Content-Length: 0\r\nContent-Encoding: identity\r\n\r\n";

    let mut single = HttpTransport::new(Cursor::new(SINGLE.as_bytes().to_vec()), Vec::new());
    assert!(
        single.read_request().is_ok(),
        "a single Content-Encoding: identity line must be accepted, or the duplicate case above \
         proves only that the parser rejects the header outright"
    );
}
