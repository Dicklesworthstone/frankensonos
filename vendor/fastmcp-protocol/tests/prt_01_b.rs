//! Exact, root-level PRT-01 B harness entries.
//!
//! These tests use only the public protocol crate surface. Every refusal below
//! travels through the shipped `admit_security_document*`, `admit_compact_jws`,
//! `admit_public_rsa_jwk`, `admit_public_jwk_set`,
//! `admit_public_rsa_components`, and `thumbprint_sha256` entry points. No
//! admission logic is re-implemented here, and no test-local validator stands
//! in for production code.
//!
//! The RFC 7638 vectors are the RFC's own Section 3.1 key and thumbprint; the
//! derived negatives (one-bit modulus flip, redundant leading zero, truncated
//! modulus, exponent 3) are computed from that same key.

use base64::Engine as _;
use fastmcp_protocol::{
    AdmittedRsaPublicJwk, CompactJwsProfile, JwkAdmissionPolicy, MAX_TOKEN_RESPONSE_BYTES,
    RawJsonAdmissionError, SecurityAdmissionError, SecurityDocumentKind, admit_compact_jws,
    admit_public_jwk_set, admit_public_rsa_components, admit_public_rsa_jwk,
    admit_security_document, admit_security_document_object,
};
use serde_json::Value;

/// RFC 7638 Section 3.1 modulus.
const RFC7638_MODULUS: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";
/// RFC 7638 Section 3.1 thumbprint of that key.
const RFC7638_THUMBPRINT: &str = "NzbLsXh8uDCcd-6MNwXF4W_7noWXFZAfHkxZsRGC9Xs";
/// The same modulus with its final bit flipped.
const FLIPPED_MODULUS: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgg";
/// The same modulus carrying one redundant leading zero octet.
const NON_MINIMAL_MODULUS: &str = "ANL8e2oKHmxnEErrj4iyV2abTfZ53a0Jm1xKbNmogBW1oTO_C4VseHG23wALVU_Os8LtUSu2jxRcboQ0dS-rUqHPwSRAj3m1ikV4wWQohVeJ96JJ44TLLZ-uLWf9lvuSbBmOB3OZ_cgVwK8Jfd5are_0TecOgn9IeEMkOb_uuWBo0EdPxQ1tkL86mN-vEEDInALWkqs7PCiWYJ2G_XO3dM4HQGR87uqjEL0S-YWo659Z_dQmzqWyEg9PKjS8q3ZLfmxU1oQCOLzEBYelnmbtHzOJRXdjXEcK91z5LCDR2kPhv8QZ4iKm8NC7NYxeOPnLBQrq_pBIFPGsGqScyp6gyoM";
/// The first 1024 bits of that modulus: canonical, but below the policy floor.
const WEAK_MODULUS: &str = "0vx7agoebGcQSuuPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGg";
/// A distinct 2048-bit modulus, for key sets that need two entries.
const SECOND_MODULUS: &str = "0vx7agoebGcQShSPiLJXZptN9nndrQmbXEps2aiAFbWhM78LhWx4cbbfAAtVT86zwu1RK7aPFFxuhDR1L6tSoc_BJECPebWKRXjBZCiFV4n3oknjhMstn64tZ_2W-5JsGY4Hc5n9yBXArwl93lqt7_RN5w6Cf0h4QyQ5v-65YGjQR0_FDW2QvzqY368QQMicAtaSqzs8KJZgnYb9c7d0zgdAZHzu6qMQvRL5hajrn1n91CbOpbISD08qNLyrdkt-bFTWhAI4vMQFh6WeZu0fM4lFd2NcRwr3XPksINHaQ-G_xBniIqbw0Ls1jF44-csFCur-kEgU8awapJzKnqDKgw";
/// SHA-256 of the whole RFC 7638 JWK JSON, which is never the thumbprint.
const WHOLE_JWK_DIGEST: &str = "1hlW0luyerGK00qKU_R4pFATPX82dR_dxS7KlBkOysQ";

fn base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Builds a compact JWS from literal header and claims JSON.
///
/// The signature segment is arbitrary: this module admits structure and never
/// verifies signatures, which is exactly the boundary under test.
fn compact_jws(header: &str, claims: &str) -> String {
    format!(
        "{}.{}.{}",
        base64url(header.as_bytes()),
        base64url(claims.as_bytes()),
        base64url(b"unverified-signature"),
    )
}

/// A canonical RFC 7638 JWK document.
fn rfc7638_jwk() -> String {
    format!(
        r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","alg":"RS256","kid":"2011-04-29"}}"#
    )
}

/// Admits a JWK document through the shipped path.
fn admit_jwk(
    policy: JwkAdmissionPolicy,
    document: &str,
) -> Result<AdmittedRsaPublicJwk, SecurityAdmissionError> {
    let members =
        admit_security_document_object(SecurityDocumentKind::JsonWebKey, document.as_bytes())?;
    admit_public_rsa_jwk(policy, &members)
}

/// One well-formed representative document per security document class.
fn representative(kind: SecurityDocumentKind) -> &'static str {
    match kind {
        SecurityDocumentKind::AuthorizationServerMetadata => {
            r#"{"issuer":"https://as.example","token_endpoint":"https://as.example/token"}"#
        }
        SecurityDocumentKind::ProtectedResourceMetadata => {
            r#"{"resource":"https://rs.example","authorization_servers":["https://as.example"]}"#
        }
        SecurityDocumentKind::OidcProviderMetadata => {
            r#"{"issuer":"https://op.example","jwks_uri":"https://op.example/jwks"}"#
        }
        SecurityDocumentKind::ClientIdMetadataDocument => {
            r#"{"client_id":"https://client.example/id","client_name":"Example"}"#
        }
        SecurityDocumentKind::ClientRegistration => {
            r#"{"redirect_uris":["https://client.example/cb"],"client_name":"Example"}"#
        }
        SecurityDocumentKind::TokenResponse => {
            r#"{"access_token":"opaque","token_type":"Bearer","expires_in":3600}"#
        }
        SecurityDocumentKind::JsonWebKeySet => r#"{"keys":[]}"#,
        SecurityDocumentKind::JsonWebKey => r#"{"kty":"RSA","n":"AQAB","e":"AQAB"}"#,
        SecurityDocumentKind::CompactJwsProtectedHeader => r#"{"alg":"RS256","typ":"at+jwt"}"#,
        SecurityDocumentKind::CompactJwsClaims => r#"{"iss":"https://as.example","sub":"user-1"}"#,
    }
}

/// A valid token for each closed profile, with its own required shape.
fn profile_token(profile: CompactJwsProfile) -> String {
    match profile {
        CompactJwsProfile::Rfc9068AccessToken => compact_jws(
            r#"{"alg":"RS256","typ":"at+jwt"}"#,
            r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"jti":"a-1","scope":"read","tenant":"acme"}"#,
        ),
        CompactJwsProfile::OidcIdToken => compact_jws(
            r#"{"alg":"RS256","typ":"JWT"}"#,
            r#"{"iss":"https://op.example","sub":"user-1","aud":"client-1","exp":1700000000,"iat":1699999000,"nonce":"n-1"}"#,
        ),
        CompactJwsProfile::IdentityAssertionJwtAuthorizationGrant => compact_jws(
            r#"{"alg":"RS256","typ":"oauth-id-jag+jwt"}"#,
            r#"{"iss":"https://op.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"scope":"read"}"#,
        ),
        CompactJwsProfile::Rfc7523ClientAssertion => compact_jws(
            r#"{"alg":"RS256","typ":"JWT"}"#,
            r#"{"iss":"client-1","sub":"client-1","aud":"https://as.example/token","exp":1700000000,"jti":"c-1"}"#,
        ),
        CompactJwsProfile::BuiltInIssuerSelfVerification => compact_jws(
            r#"{"alg":"RS256","typ":"JWT"}"#,
            r#"{"iss":"https://self.example","sub":"https://self.example","aud":"https://self.example","exp":1700000000,"iat":1699999000}"#,
        ),
    }
}

#[test]
fn prt_01_security_admission_positive() {
    // Every named security-bearing consumer reaches the same bounded pass.
    for kind in SecurityDocumentKind::all() {
        let document = representative(kind);
        assert!(
            admit_security_document(kind, document.as_bytes()).is_ok(),
            "{kind:?} must admit its well-formed representative document",
        );
        let members = admit_security_document_object(kind, document.as_bytes())
            .expect("an admitted document materializes its members");
        assert!(
            !members.is_empty(),
            "{kind:?} materializes the admitted members",
        );
    }

    // The materialized object is exactly the admitted document, decoded after
    // admission rather than before it.
    let token = admit_security_document_object(
        SecurityDocumentKind::TokenResponse,
        representative(SecurityDocumentKind::TokenResponse).as_bytes(),
    )
    .expect("a token response is admitted");
    assert_eq!(
        token.get("token_type"),
        Some(&Value::String("Bearer".to_owned())),
    );
    assert_eq!(
        token.get("expires_in").and_then(serde_json::Value::as_u64),
        Some(3600),
    );

    // A document sitting exactly on its class bound is admitted; the bound is
    // the class's own, not a global one.
    let padding =
        "x".repeat(MAX_TOKEN_RESPONSE_BYTES - r#"{"access_token":"","token_type":"Bearer"}"#.len());
    let at_limit = format!(r#"{{"access_token":"{padding}","token_type":"Bearer"}}"#);
    assert_eq!(at_limit.len(), MAX_TOKEN_RESPONSE_BYTES);
    assert!(
        admit_security_document(SecurityDocumentKind::TokenResponse, at_limit.as_bytes()).is_ok(),
        "a document exactly on its bound is admitted",
    );
}

#[test]
fn prt_01_security_admission_planted_negative() {
    // Every class refuses a duplicate member, one variable off its own
    // admitted representative.
    for kind in SecurityDocumentKind::all() {
        let baseline = representative(kind);
        assert!(admit_security_document(kind, baseline.as_bytes()).is_ok());

        let planted = format!(r#"{{"dup":1,"dup":2,"trailer":{}}}"#, baseline.len());
        let failure = admit_security_document(kind, planted.as_bytes())
            .expect_err("a duplicate member is refused for every document class");
        assert_eq!(
            failure.error(),
            RawJsonAdmissionError::DuplicateObjectMember
        );
        assert_eq!(failure.path(), "/dup");

        // Refusal left the class able to admit its baseline again.
        assert!(admit_security_document(kind, baseline.as_bytes()).is_ok());
    }

    // Two typed consumers cannot disagree about one document: the identical
    // duplicate is refused identically under two different classes, so neither
    // can observe a last-member-wins reading the other did not.
    let ambiguous = br#"{"keys":[],"keys":[{"kty":"oct"}]}"#;
    let as_key_set = admit_security_document(SecurityDocumentKind::JsonWebKeySet, ambiguous)
        .expect_err("a duplicate keys member is refused");
    let as_token = admit_security_document(SecurityDocumentKind::TokenResponse, ambiguous)
        .expect_err("the identical document is refused for the other consumer");
    assert_eq!(as_key_set, as_token, "both consumers see one verdict");
    assert_eq!(
        as_key_set.error(),
        RawJsonAdmissionError::DuplicateObjectMember,
    );

    // The rest of the bounded pass applies unchanged to security documents.
    let kind = SecurityDocumentKind::TokenResponse;
    let over_limit = "x".repeat(MAX_TOKEN_RESPONSE_BYTES + 1);
    let planted: [(&str, Vec<u8>, RawJsonAdmissionError); 6] = [
        (
            "byte-order mark",
            [&[0xef, 0xbb, 0xbf][..], br#"{"a":1}"#].concat(),
            RawJsonAdmissionError::ByteOrderMark,
        ),
        (
            "malformed UTF-8",
            [&br#"{"a":""#[..], &[0xc3][..], &br#"b"}"#[..]].concat(),
            RawJsonAdmissionError::InvalidUtf8,
        ),
        (
            "top-level array",
            br#"[{"a":1}]"#.to_vec(),
            RawJsonAdmissionError::TopLevelNotObject,
        ),
        (
            "document over its class bound",
            over_limit.into_bytes(),
            RawJsonAdmissionError::DocumentTooLarge,
        ),
        (
            "oversized numeric lexeme",
            br#"{"a":1e999999}"#.to_vec(),
            RawJsonAdmissionError::ExponentTooLarge,
        ),
        (
            "over-deep nesting",
            format!("{}1{}", "{\"a\":".repeat(65), "}".repeat(65)).into_bytes(),
            RawJsonAdmissionError::NestingTooDeep,
        ),
    ];
    for (label, bytes, expected) in planted {
        let failure = admit_security_document(kind, &bytes)
            .expect_err("the bounded pass refuses this security document");
        assert_eq!(failure.error(), expected, "{label} must be refused by name");
    }

    // A top-level array is an ordinary shape violation for a security
    // document; it never inherits the JSON-RPC batch vocabulary.
    let array = admit_security_document(kind, br#"[{"a":1}]"#)
        .expect_err("a security document is a single object");
    assert_ne!(array.error(), RawJsonAdmissionError::TopLevelBatch);
}

#[test]
fn prt_01_compact_jws_profiles_positive() {
    for profile in CompactJwsProfile::all() {
        let token = profile_token(profile);
        let admitted = admit_compact_jws(profile, &token)
            .unwrap_or_else(|error| panic!("{profile:?} must admit its own token: {error}"));
        assert_eq!(admitted.profile(), profile);
        assert_eq!(admitted.header().algorithm(), "RS256");
        assert_eq!(admitted.signature(), b"unverified-signature".as_slice());
        // The signing input is the exact first two segments, unmodified.
        assert!(token.starts_with(admitted.signing_input()));
        assert_eq!(admitted.signing_input().matches('.').count(), 1);
        for required in profile.required_claims() {
            assert!(
                admitted.claims().contains_key(*required),
                "{profile:?} retains its required claim {required}",
            );
        }
    }

    // RFC 7515 Section 4.1.9: the shortened and full `application/` forms and
    // their ASCII case variants are one semantic type, and canonical emission
    // stays shortened lowercase.
    let profile = CompactJwsProfile::IdentityAssertionJwtAuthorizationGrant;
    let claims = r#"{"iss":"https://op.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000}"#;
    for spelling in [
        "oauth-id-jag+jwt",
        "application/oauth-id-jag+jwt",
        "OAuth-ID-JAG+JWT",
        "application/OAUTH-ID-JAG+JWT",
        "APPLICATION/oauth-id-jag+jwt",
    ] {
        let token = compact_jws(&format!(r#"{{"alg":"RS256","typ":"{spelling}"}}"#), claims);
        let admitted = admit_compact_jws(profile, &token)
            .unwrap_or_else(|error| panic!("{spelling} must map to one type: {error}"));
        assert_eq!(
            admitted.header().media_type(),
            Some("oauth-id-jag+jwt"),
            "{spelling} normalizes to the canonical shortened lowercase form",
        );
    }
    assert_eq!(profile.canonical_media_type(), Some("oauth-id-jag+jwt"));

    // Bounded unknown claims are syntactically admitted and inert. This is not
    // a closed claim-name allowlist: `tenant` survives in the claim set but
    // can never be read as authorization input.
    let access = admit_compact_jws(
        CompactJwsProfile::Rfc9068AccessToken,
        &profile_token(CompactJwsProfile::Rfc9068AccessToken),
    )
    .expect("the access token is admitted");
    assert_eq!(
        access.claims().get("tenant"),
        Some(&Value::String("acme".to_owned())),
        "an unknown claim is retained",
    );
    assert_eq!(
        access.authorization_claim("tenant"),
        None,
        "an unnamed claim is inert as authorization input",
    );
    assert_eq!(
        access.authorization_claim("scope"),
        Some(&Value::String("read".to_owned())),
        "a claim this profile names authorization-relevant is readable",
    );
}

#[test]
fn prt_01_compact_jws_profiles_planted_negative() {
    // Full cross-profile confusion matrix: bytes valid under one profile are
    // refused under every other, with only the selected profile changed.
    for minted in CompactJwsProfile::all() {
        let token = profile_token(minted);
        assert!(
            admit_compact_jws(minted, &token).is_ok(),
            "{minted:?} admits its own token",
        );
        for reinterpreted in CompactJwsProfile::all() {
            if reinterpreted == minted {
                continue;
            }
            assert!(
                admit_compact_jws(reinterpreted, &token).is_err(),
                "{minted:?} bytes must never be admitted as {reinterpreted:?}",
            );
        }
        // The failed reinterpretations left the original profile intact.
        assert!(admit_compact_jws(minted, &token).is_ok());
    }

    // The three profiles that legitimately carry `JWT` are separated by their
    // issuer/subject/audience relationship, not by the media type alone.
    let id_token_shape = r#"{"iss":"https://op.example","sub":"user-1","aud":"client-1","exp":1700000000,"iat":1699999000}"#;
    let token = compact_jws(r#"{"alg":"RS256","typ":"JWT"}"#, id_token_shape);
    assert!(
        admit_compact_jws(CompactJwsProfile::OidcIdToken, &token).is_ok(),
        "an issuer-asserts-user token is an ID token",
    );
    assert_eq!(
        admit_compact_jws(CompactJwsProfile::BuiltInIssuerSelfVerification, &token),
        Err(SecurityAdmissionError::CrossProfileConfusion),
        "only the subject relationship differs, and it decides the profile",
    );

    // One variable at a time against the admitted access token.
    let claims = r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"jti":"a-1"}"#;
    let profile = CompactJwsProfile::Rfc9068AccessToken;
    assert!(
        admit_compact_jws(
            profile,
            &compact_jws(r#"{"alg":"RS256","typ":"at+jwt"}"#, claims)
        )
        .is_ok(),
        "the unmodified access token is admitted",
    );

    let planted: [(&str, String, SecurityAdmissionError); 7] = [
        (
            "a media-type parameter never enters the admitted variant",
            compact_jws(r#"{"alg":"RS256","typ":"at+jwt;charset=utf-8"}"#, claims),
            SecurityAdmissionError::ProfileTypeMismatch,
        ),
        (
            "a different subtype is a different type",
            compact_jws(r#"{"alg":"RS256","typ":"at+jwt-v2"}"#, claims),
            SecurityAdmissionError::ProfileTypeMismatch,
        ),
        (
            "a nested media type never normalizes",
            compact_jws(
                r#"{"alg":"RS256","typ":"application/example/at+jwt"}"#,
                claims,
            ),
            SecurityAdmissionError::ProfileTypeMismatch,
        ),
        (
            "this profile's media type is mandatory",
            compact_jws(r#"{"alg":"RS256"}"#, claims),
            SecurityAdmissionError::ProfileTypeMismatch,
        ),
        (
            "alg none would make every signature check vacuous",
            compact_jws(r#"{"alg":"none","typ":"at+jwt"}"#, claims),
            SecurityAdmissionError::UnsupportedAlgorithm,
        ),
        (
            "an ID-token binding claim is forbidden in an access token",
            compact_jws(
                r#"{"alg":"RS256","typ":"at+jwt"}"#,
                r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"jti":"a-1","nonce":"n-1"}"#,
            ),
            SecurityAdmissionError::ForbiddenClaim("nonce"),
        ),
        (
            "a NumericDate claim is a number, never a string",
            compact_jws(
                r#"{"alg":"RS256","typ":"at+jwt"}"#,
                r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":"1700000000","iat":1699999000,"jti":"a-1"}"#,
            ),
            SecurityAdmissionError::MissingOrMalformedClaim("exp"),
        ),
    ];
    for (label, token, expected) in planted {
        assert_eq!(admit_compact_jws(profile, &token), Err(expected), "{label}");
    }

    // Structural refusals reach the same boundary before any claim is read.
    assert_eq!(
        admit_compact_jws(profile, "only.two"),
        Err(SecurityAdmissionError::MalformedCompactSerialization),
    );
    assert_eq!(
        admit_compact_jws(profile, "a.b.c.d"),
        Err(SecurityAdmissionError::MalformedCompactSerialization),
    );
    // A duplicate member in the protected header is refused by the same
    // bounded pass the envelope slice uses.
    let duplicated = compact_jws(r#"{"alg":"RS256","alg":"none","typ":"at+jwt"}"#, claims);
    assert!(matches!(
        admit_compact_jws(profile, &duplicated),
        Err(SecurityAdmissionError::Document(_)),
    ));

    // The admitted baseline still admits after every refusal above.
    assert!(
        admit_compact_jws(
            profile,
            &compact_jws(r#"{"alg":"RS256","typ":"at+jwt"}"#, claims)
        )
        .is_ok(),
    );
}

#[test]
fn prt_01_jwk_policy_positive() {
    let policy = JwkAdmissionPolicy::default();
    let key = admit_jwk(policy, &rfc7638_jwk()).expect("the RFC 7638 public key is admitted");

    // Canonical public parameters are exact.
    assert_eq!(key.modulus().len(), 256, "a 2048-bit modulus is 256 octets");
    assert_ne!(key.modulus()[0], 0, "no redundant leading zero survives");
    assert_eq!(
        key.exponent(),
        &[0x01, 0x00, 0x01],
        "exponent 65537 is exactly 01 00 01",
    );
    assert_eq!(base64url(key.exponent()), "AQAB");
    assert_eq!(key.key_id(), Some("2011-04-29"));
    assert_eq!(key.algorithm(), Some("RS256"));

    // Explicit signing usage is admitted in both spellings.
    for usage in [
        r#","use":"sig""#,
        r#","key_ops":["verify"]"#,
        r#","use":"sig","key_ops":["verify"]"#,
    ] {
        let document =
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k1"{usage}}}"#);
        assert!(
            admit_jwk(policy, &document).is_ok(),
            "signing usage {usage} is admitted",
        );
    }

    // A key set admits distinct identifiers and preserves order.
    let key_set = format!(
        r#"{{"keys":[{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"a"}},{{"kty":"RSA","n":"{SECOND_MODULUS}","e":"AQAB","kid":"b"}}]}}"#
    );
    let admitted = admit_public_jwk_set(policy, key_set.as_bytes())
        .expect("two distinctly identified keys are admitted");
    assert_eq!(admitted.len(), 2);
    assert_eq!(admitted[0].key_id(), Some("a"));
    assert_eq!(admitted[1].key_id(), Some("b"));
    assert_ne!(admitted[0].modulus(), admitted[1].modulus());

    // A deployment may drop the kid requirement, and may only raise the
    // strength floor, never lower it.
    let anonymous = format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB"}}"#);
    assert!(admit_jwk(policy.without_required_key_id(), &anonymous).is_ok());
    assert_eq!(
        policy
            .with_minimum_rsa_modulus_bytes(1)
            .minimum_rsa_modulus_bytes(),
        policy.minimum_rsa_modulus_bytes(),
        "the compiled strength floor cannot be lowered by a policy",
    );
    assert_eq!(
        policy
            .with_minimum_rsa_modulus_bytes(384)
            .minimum_rsa_modulus_bytes(),
        384,
        "a deployment may demand stronger keys",
    );
}

#[test]
fn prt_01_jwk_policy_planted_negative() {
    let policy = JwkAdmissionPolicy::default();
    let baseline = rfc7638_jwk();
    assert!(admit_jwk(policy, &baseline).is_ok());

    let planted: [(&str, String, SecurityAdmissionError); 11] = [
        (
            "a private exponent is never admitted",
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","d":"AQAB"}}"#),
            SecurityAdmissionError::NonPublicKeyMaterial,
        ),
        (
            "a private prime is never admitted",
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","p":"AQAB"}}"#),
            SecurityAdmissionError::NonPublicKeyMaterial,
        ),
        (
            "symmetric material is never admitted",
            r#"{"kty":"oct","k":"AQAB","kid":"k"}"#.to_owned(),
            SecurityAdmissionError::NonPublicKeyMaterial,
        ),
        (
            "an encryption-only key cannot verify",
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","use":"enc"}}"#),
            SecurityAdmissionError::NotAVerificationKey,
        ),
        (
            "a signing operation implies private material",
            format!(
                r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","key_ops":["sign","verify"]}}"#
            ),
            SecurityAdmissionError::NonPublicKeyMaterial,
        ),
        (
            "key_ops without verify cannot verify",
            format!(
                r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","key_ops":["encrypt"]}}"#
            ),
            SecurityAdmissionError::NotAVerificationKey,
        ),
        (
            "a signing use beside an encryption operation is ambiguous",
            format!(
                r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"k","use":"sig","key_ops":["verify","encrypt"]}}"#
            ),
            SecurityAdmissionError::AmbiguousKeyUsage,
        ),
        (
            "a 1024-bit modulus is below the policy floor",
            format!(r#"{{"kty":"RSA","n":"{WEAK_MODULUS}","e":"AQAB","kid":"k"}}"#),
            SecurityAdmissionError::UnsupportedKeyStrength,
        ),
        (
            "exponent 3 is not the admitted exponent",
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"Aw","kid":"k"}}"#),
            SecurityAdmissionError::UnsupportedKeyStrength,
        ),
        (
            "a redundant leading zero is not a canonical Base64urlUInt",
            format!(r#"{{"kty":"RSA","n":"{NON_MINIMAL_MODULUS}","e":"AQAB","kid":"k"}}"#),
            SecurityAdmissionError::MalformedJwkMember("n"),
        ),
        (
            "a kid is required by default",
            format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB"}}"#),
            SecurityAdmissionError::MalformedJwkMember("kid"),
        ),
    ];
    for (label, document, expected) in planted {
        assert_eq!(admit_jwk(policy, &document), Err(expected), "{label}");
    }

    // Padded base64 is not canonical unpadded base64url.
    let padded = format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB=","kid":"k"}}"#);
    assert_eq!(
        admit_jwk(policy, &padded),
        Err(SecurityAdmissionError::MalformedJwkMember("e")),
    );

    // One key set, two identical identifiers.
    let colliding = format!(
        r#"{{"keys":[{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","kid":"same"}},{{"kty":"RSA","n":"{SECOND_MODULUS}","e":"AQAB","kid":"same"}}]}}"#
    );
    assert_eq!(
        admit_public_jwk_set(policy, colliding.as_bytes()),
        Err(SecurityAdmissionError::DuplicateKeyIdentifier),
    );

    // Every refusal above left the baseline admissible and unchanged.
    let readmitted = admit_jwk(policy, &baseline).expect("the baseline still admits");
    assert_eq!(readmitted.key_id(), Some("2011-04-29"));
    assert_eq!(base64url(readmitted.modulus()), RFC7638_MODULUS);
}

#[test]
fn prt_01_rfc7638_thumbprint_positive() {
    let policy = JwkAdmissionPolicy::default();
    let key = admit_jwk(policy, &rfc7638_jwk()).expect("the RFC 7638 key is admitted");

    // RFC 7638 Section 3.1 known answer.
    let thumbprint = key
        .thumbprint_sha256()
        .expect("the thumbprint is computable");
    assert_eq!(thumbprint.to_base64url(), RFC7638_THUMBPRINT);
    assert_eq!(thumbprint.as_bytes().len(), 32);
    assert_eq!(thumbprint.to_string(), RFC7638_THUMBPRINT);

    // The hashed bytes are exactly the required members in lexicographic order
    // with no whitespace.
    assert_eq!(
        key.rfc7638_canonical_input(),
        format!(r#"{{"e":"AQAB","kty":"RSA","n":"{RFC7638_MODULUS}"}}"#),
    );

    // Equivalent encodings converge. A different JWK member order, extra
    // whitespace, and additional members are all the same key.
    let reordered = format!(
        "{{\n  \"kid\" : \"2011-04-29\",\n  \"e\":\"AQAB\",\n  \"alg\":\"RS256\",\n  \"n\":\"{RFC7638_MODULUS}\",\n  \"kty\":\"RSA\"\n}}"
    );
    let same = admit_jwk(policy, &reordered).expect("member order does not change the key");
    assert_eq!(
        same.thumbprint_sha256().expect("computable").to_base64url(),
        RFC7638_THUMBPRINT,
    );

    // A DER, SPKI, or KMS export commonly carries a redundant leading zero
    // octet. Normalizing through the components entry point converges on the
    // same admitted key and therefore the same thumbprint.
    let raw_modulus = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(RFC7638_MODULUS)
        .expect("the RFC modulus decodes");
    let der_style = [&[0x00_u8][..], &raw_modulus[..]].concat();
    let normalized = admit_public_rsa_components(
        policy,
        &der_style,
        &[0x00, 0x01, 0x00, 0x01],
        Some("2011-04-29"),
    )
    .expect("a DER-style export normalizes to the admitted public parameters");
    assert_eq!(normalized.modulus(), key.modulus());
    assert_eq!(normalized.exponent(), key.exponent());
    assert_eq!(
        normalized
            .thumbprint_sha256()
            .expect("computable")
            .to_base64url(),
        RFC7638_THUMBPRINT,
    );
}

#[test]
fn prt_01_rfc7638_thumbprint_planted_negative() {
    let policy = JwkAdmissionPolicy::default();
    let baseline = admit_jwk(policy, &rfc7638_jwk()).expect("the RFC 7638 key is admitted");
    let expected = baseline.thumbprint_sha256().expect("computable");
    assert_eq!(expected.to_base64url(), RFC7638_THUMBPRINT);

    // One flipped bit in the modulus is a different key.
    let flipped = admit_jwk(
        policy,
        &format!(r#"{{"kty":"RSA","n":"{FLIPPED_MODULUS}","e":"AQAB","kid":"2011-04-29"}}"#),
    )
    .expect("the one-bit-different key is still a well-formed key");
    let flipped_thumbprint = flipped.thumbprint_sha256().expect("computable");
    assert_ne!(
        flipped_thumbprint.to_base64url(),
        RFC7638_THUMBPRINT,
        "a one-bit modulus change must diverge",
    );
    assert_ne!(flipped.modulus(), baseline.modulus());

    // A different exponent is a different key, and here is not even admitted.
    assert_eq!(
        admit_jwk(
            policy,
            &format!(r#"{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"Aw","kid":"k"}}"#)
        ),
        Err(SecurityAdmissionError::UnsupportedKeyStrength),
    );

    // A non-minimal integer is refused rather than silently normalized on the
    // JWK path, so a peer cannot offer two spellings of one key.
    assert_eq!(
        admit_jwk(
            policy,
            &format!(r#"{{"kty":"RSA","n":"{NON_MINIMAL_MODULUS}","e":"AQAB","kid":"k"}}"#)
        ),
        Err(SecurityAdmissionError::MalformedJwkMember("n")),
    );

    // The digest of a whole JWK serialization is never the thumbprint. If
    // identity were ever taken from the document rather than the canonical
    // members, this is the value it would produce.
    assert_ne!(
        expected.to_base64url(),
        WHOLE_JWK_DIGEST,
        "identity comes from the canonical members, never from the JWK document",
    );

    // Nothing above changed the admitted key or its thumbprint.
    let readmitted = admit_jwk(policy, &rfc7638_jwk()).expect("the baseline still admits");
    assert_eq!(
        readmitted.thumbprint_sha256().expect("computable"),
        expected,
    );
}

#[test]
fn prt_01_b_positive() {
    // One end-to-end pass across the whole B slice through the shipped public
    // surface: a JWKS is admitted under the bounded security-document pass,
    // its key yields the exact RFC 7638 thumbprint, and a compact JWS naming
    // that key is admitted under exactly one closed profile.
    let policy = JwkAdmissionPolicy::default();
    let key_set = format!(
        r#"{{"keys":[{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","alg":"RS256","use":"sig","kid":"2011-04-29"}}]}}"#
    );
    let keys = admit_public_jwk_set(policy, key_set.as_bytes())
        .expect("the shipped JWKS admission accepts a well-formed public key set");
    assert_eq!(keys.len(), 1);
    assert_eq!(
        keys[0]
            .thumbprint_sha256()
            .expect("computable")
            .to_base64url(),
        RFC7638_THUMBPRINT,
    );

    let token = compact_jws(
        r#"{"alg":"RS256","typ":"at+jwt","kid":"2011-04-29"}"#,
        r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"jti":"a-1","scope":"read"}"#,
    );
    let admitted = admit_compact_jws(CompactJwsProfile::Rfc9068AccessToken, &token)
        .expect("the shipped compact-JWS admission accepts an RFC 9068 access token");
    assert_eq!(admitted.header().key_id(), keys[0].key_id());
    assert_eq!(admitted.header().media_type(), Some("at+jwt"));
    assert_eq!(
        admitted.authorization_claim("scope"),
        Some(&Value::String("read".to_owned())),
    );
    // Admission stops before verification: the signature is handed on, never
    // checked here.
    assert_eq!(admitted.signature(), b"unverified-signature".as_slice());
}

#[test]
fn prt_01_b_planted_negative() {
    let policy = JwkAdmissionPolicy::default();
    let key_set = format!(
        r#"{{"keys":[{{"kty":"RSA","n":"{RFC7638_MODULUS}","e":"AQAB","alg":"RS256","use":"sig","kid":"2011-04-29"}}]}}"#
    );
    assert!(admit_public_jwk_set(policy, key_set.as_bytes()).is_ok());

    // Only the key's usage changes: the identical key set becomes an
    // encryption-only key and is refused before any thumbprint exists.
    let encryption_only = key_set.replace(r#""use":"sig""#, r#""use":"enc""#);
    assert_eq!(
        admit_public_jwk_set(policy, encryption_only.as_bytes()),
        Err(SecurityAdmissionError::NotAVerificationKey),
    );

    // Only the selected profile changes: bytes minted as an RFC 9068 access
    // token cannot be reinterpreted as an OIDC ID token.
    let token = compact_jws(
        r#"{"alg":"RS256","typ":"at+jwt","kid":"2011-04-29"}"#,
        r#"{"iss":"https://as.example","sub":"user-1","aud":"https://rs.example","exp":1700000000,"iat":1699999000,"jti":"a-1","scope":"read"}"#,
    );
    assert!(admit_compact_jws(CompactJwsProfile::Rfc9068AccessToken, &token).is_ok());
    assert_eq!(
        admit_compact_jws(CompactJwsProfile::OidcIdToken, &token),
        Err(SecurityAdmissionError::ProfileTypeMismatch),
    );

    // The admitted key set and token are unchanged by the refusals.
    let keys = admit_public_jwk_set(policy, key_set.as_bytes()).expect("the key set still admits");
    assert_eq!(
        keys[0]
            .thumbprint_sha256()
            .expect("computable")
            .to_base64url(),
        RFC7638_THUMBPRINT,
    );
    assert!(admit_compact_jws(CompactJwsProfile::Rfc9068AccessToken, &token).is_ok());
}
