//! FND-05 (bd-ho7of items 2 and 6): REDIRECT-ORIGIN INTERCEPTION, proven from
//! OUTSIDE the crate through the shipped public surface.
//!
//! HOW IT REACHES THE POLICY. `guarded_admit_native_response` is the whole
//! guarded response-admission policy, including redirect interception. It is
//! public precisely so this proof does not have to be a `#[cfg(test)]` one:
//! `cfg(test)` holds only while compiling the crate's own lib-test harness, so
//! a proof written there is unreachable to every integration test and every
//! downstream consumer, and is inadmissible under PL-3. This file is an
//! external consumer — it links the crate the way any dependent does.
//!
//! WHAT "INTERCEPTION" MEANS HERE, and it is two claims, not one:
//!   1. a 3xx is SURFACED as typed data rather than silently succeeding, and
//!   2. the redirect target NEVER BECOMES THE ORIGIN of the response.
//!
//! Claim 2 is the security-relevant half. A client that quietly adopted the
//! redirect's host would report provenance for a peer it never validated a
//! certificate against, which is exactly the redirect-origin confusion that
//! guarded OAuth discovery must not be exposed to.
//!
//! WHAT THIS FILE DOES NOT PROVE, stated here so the gap is not inferred from
//! its absence: it does not establish that `GuardedHttpFetcher::fetch` declines
//! to OPEN A FOLLOW-UP CONNECTION to the redirect target. That is a property of
//! the caller of this function, not of this function, and proving it from
//! outside requires a served 3xx over TLS — which in turn requires admitting a
//! non-public peer, i.e. defeating the address fence. It remains proven only by
//! the in-crate `rh5_guarded_loopback_real_wire_redirect_is_typed_without_follow_up`.

use std::net::SocketAddr;

use fastmcp_transport::http::{
    GuardedHttpFetchError, GuardedHttpFetchResponse, GuardedHttpPeerProvenance, GuardedHttpRedirect,
    guarded_admit_native_response,
};

const ORIGIN_HOST: &str = "origin.example";
const REDIRECT_TARGET: &str = "https://other.example/next";

/// Provenance for a fetch that already completed against `ORIGIN_HOST`. Every
/// field is fixed so that any mutation by the admission path is visible.
fn origin_provenance() -> GuardedHttpPeerProvenance {
    GuardedHttpPeerProvenance {
        host: ORIGIN_HOST.to_owned(),
        selected_address: "93.184.216.34:443"
            .parse::<SocketAddr>()
            .expect("fixed public test address"),
        leaf_certificate_sha256: [7_u8; 32],
        alpn: Some(b"http/1.1".to_vec()),
        tls_protocol: Some("TLSv1.3".to_owned()),
        root_policy_revision: "webpki-fixed".to_owned(),
    }
}

fn admit(
    status: u16,
    headers: &[(&str, &str)],
) -> Result<GuardedHttpFetchResponse, GuardedHttpFetchError> {
    guarded_admit_native_response(
        status,
        headers
            .iter()
            .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
            .collect(),
        Vec::new(),
        1024,
        origin_provenance(),
    )
}

#[test]
fn fnd_05_redirect_is_surfaced_as_data_and_never_becomes_the_origin_positive() {
    let response = admit(302, &[("Location", REDIRECT_TARGET), ("Content-Length", "0")])
        .expect("a 302 is an admitted response, not a transport error");

    // CLAIM 1 — the redirect is surfaced, typed, carrying its target.
    assert_eq!(
        response.redirect,
        Some(GuardedHttpRedirect {
            status: 302,
            location: Some(REDIRECT_TARGET.to_owned()),
        }),
        "a 3xx must be reported to the caller as typed redirect data"
    );

    // CLAIM 2 — INTERCEPTION. The redirect target must not be adopted as the
    // origin. Compared field-by-field against the input, so adopting ANY part
    // of the redirect (host, address, certificate identity) fails here.
    assert_eq!(
        response.provenance,
        origin_provenance(),
        "redirect-origin interception: provenance must pass through unchanged, \
         so the redirect target never becomes the origin of this response"
    );
    assert_eq!(
        response.provenance.host, ORIGIN_HOST,
        "the validated peer, not the redirect target, is the origin"
    );

    // The status is reported as observed, not normalised into a success.
    assert_eq!(response.status, 302);
}

#[test]
fn fnd_05_redirect_interception_planted_negatives() {
    // A worthless implementation that reports a redirect whenever a Location
    // header is present would pass the positive above. It fails here.
    let with_location_on_success =
        admit(200, &[("Location", REDIRECT_TARGET)]).expect("a 200 is admitted");
    assert_eq!(
        with_location_on_success.redirect, None,
        "a Location header on a non-3xx must NOT manufacture a redirect"
    );

    // The redirect class is exactly 300..400. The 299/300 and 399/400 pairs
    // differ by one in the status alone and demand OPPOSITE outcomes, so no
    // implementation with a constant answer can satisfy both halves.
    for (status, expected_redirect) in [(299_u16, false), (300, true), (399, true), (400, false)] {
        let response =
            admit(status, &[("Location", REDIRECT_TARGET)]).expect("status is admitted");
        assert_eq!(
            response.redirect.is_some(),
            expected_redirect,
            "status {status}: redirect class must be exactly 300..400"
        );
        // Whatever the classification, the origin is never the redirect target.
        assert_eq!(
            response.provenance.host, ORIGIN_HOST,
            "status {status}: origin must survive classification"
        );
    }

    // A redirect WITHOUT a Location is still surfaced, so a caller cannot
    // mistake it for a successful body-bearing response.
    assert_eq!(
        admit(302, &[]).expect("admitted").redirect,
        Some(GuardedHttpRedirect {
            status: 302,
            location: None,
        }),
        "a 3xx with no Location must still be reported AS a redirect"
    );

    // Header field names are case-insensitive on the wire; a case-sensitive
    // lookup would silently drop the target and report `location: None`.
    assert_eq!(
        admit(307, &[("LOCATION", REDIRECT_TARGET)])
            .expect("admitted")
            .redirect
            .and_then(|redirect| redirect.location)
            .as_deref(),
        Some(REDIRECT_TARGET),
        "the Location lookup must be case-insensitive"
    );
}
