//! FND-05 A item 9: ambiguous request framing is rejected, proven from OUTSIDE.
//!
//! The package body requires "Prove ambiguous framing rejection". The
//! capability already ships — `read_request_inner` refuses a request carrying
//! both `Content-Length` and `Transfer-Encoding`, which is the classic
//! request-smuggling desync. What did NOT exist was an admissible proof: the
//! only coverage was an in-crate `#[cfg(test)]` unit test, and this bead's
//! acceptance states plainly that `cfg(test)` behaviour cannot prove shipped
//! behaviour (PL-3).
//!
//! This reaches the same refusal as an external consumer, through
//! `fastmcp_transport::http::HttpTransport` — reachable because `lib.rs`
//! declares `pub mod http`, even though `HttpTransport` is absent from the
//! flat re-export list. No API was widened to make this proof possible.

#![forbid(unsafe_code)]

use std::io::Cursor;

use fastmcp_transport::http::{HttpError, HttpTransport};

/// Feeds one raw request through the shipped reader and returns the outcome.
fn read_raw(raw: &str) -> Result<(), HttpError> {
    let mut transport = HttpTransport::new(Cursor::new(raw.as_bytes().to_vec()), Vec::new());
    transport.read_request().map(|_| ())
}

/// True when the transport refused this request with a header-level error.
fn refused_as_invalid_header(raw: &str) -> bool {
    matches!(read_raw(raw), Err(HttpError::InvalidHeader(_)))
}

/// POSITIVE: a request carrying BOTH framing fields is refused.
///
/// `Content-Length` and `Transfer-Encoding` together leave the message length
/// ambiguous, which is the desync primitive behind request smuggling. The
/// refusal must happen at admission, before any body is processed.
#[test]
fn fnd_05_ambiguous_framing_is_rejected_positive() {
    const AMBIGUOUS: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\nContent-Length: 0\r\n\
         Transfer-Encoding: chunked\r\n\r\n";

    assert!(
        refused_as_invalid_header(AMBIGUOUS),
        "a request carrying both Content-Length and Transfer-Encoding must be refused at \
         admission; admitting it is the request-smuggling desync this check exists to stop. \
         Got {:?}",
        read_raw(AMBIGUOUS)
    );

    // Field order must not matter: the same ambiguity stated the other way round
    // is refused identically. A parser that only inspected the first framing
    // field it met would pass the case above and fail this one.
    const AMBIGUOUS_REVERSED: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\nTransfer-Encoding: chunked\r\n\
         Content-Length: 0\r\n\r\n";

    assert!(
        refused_as_invalid_header(AMBIGUOUS_REVERSED),
        "the ambiguity must be refused regardless of which framing field appears first; \
         order-sensitivity here would be a bypass. Got {:?}",
        read_raw(AMBIGUOUS_REVERSED)
    );
}

/// PLANTED NEGATIVE: each framing field ALONE is accepted.
///
/// This is the discriminator, and without it the positive proves nothing. A
/// transport that rejected every request — or every request mentioning either
/// header — would satisfy the positive just as well as a correct one. Removing
/// exactly one of the two fields, changing nothing else, must flip the outcome
/// from refusal to acceptance.
#[test]
fn fnd_05_ambiguous_framing_planted_negative() {
    // Mutation 1: drop Transfer-Encoding, keep Content-Length. Unambiguous.
    const CONTENT_LENGTH_ONLY: &str =
        "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\nContent-Length: 0\r\n\r\n";

    assert!(
        read_raw(CONTENT_LENGTH_ONLY).is_ok(),
        "Content-Length alone is unambiguous and must be admitted; refusing it would mean the \
         positive above is satisfied by a transport that rejects everything. Got {:?}",
        read_raw(CONTENT_LENGTH_ONLY)
    );

    // Mutation 2: drop Content-Length, keep Transfer-Encoding with a complete
    // zero-length chunked body. Also unambiguous.
    const TRANSFER_ENCODING_ONLY: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\nTransfer-Encoding: chunked\r\n\r\n\
         0\r\n\r\n";

    assert!(
        read_raw(TRANSFER_ENCODING_ONLY).is_ok(),
        "Transfer-Encoding alone is unambiguous and must be admitted. Got {:?}",
        read_raw(TRANSFER_ENCODING_ONLY)
    );

    // The discriminator, stated as an assertion rather than left implicit:
    // the SAME request refused above becomes acceptable when exactly one
    // framing field is removed. The refusal therefore tracks the AMBIGUITY,
    // not the mere presence of either header.
    const AMBIGUOUS: &str = "POST /mcp/v1 HTTP/1.1\r\nHost: example.invalid\r\nContent-Length: 0\r\n\
         Transfer-Encoding: chunked\r\n\r\n";
    assert!(refused_as_invalid_header(AMBIGUOUS));
    assert!(read_raw(CONTENT_LENGTH_ONLY).is_ok());
}
