//! Reusable bounded admission for security-bearing JSON, compact JWS, and JWK
//! material.
//!
//! This module parses and validates bytes and typed values. It performs no
//! HTTP, no OAuth or OIDC flow orchestration, no key discovery or lookup, and
//! no signing or signature verification. Higher role crates (the built-in
//! authorization server, the client's OAuth machinery, and the FND-09 signing
//! ring) import this one implementation; none of them import each other.
//!
//! Every JSON boundary here reuses [`crate::jsonrpc::admit_raw_json_document`],
//! the same bounded streaming pass the JSON-RPC envelope slice uses, so a
//! duplicate member, an over-deep document, an oversized numeric lexeme, a
//! malformed UTF-8 sequence, or a byte-order mark is refused identically
//! whether it arrives as a JSON-RPC frame or as a JWKS.

use std::collections::BTreeSet;

use base64::Engine as _;
use serde_json::{Map, Value};

use crate::jsonrpc::{RawJsonAdmissionError, RawJsonAdmissionFailure, RawJsonTopLevel};

/// Maximum bytes in an OAuth 2.0 authorization server metadata document.
pub const MAX_OAUTH_METADATA_BYTES: usize = 64 * 1024;
/// Maximum bytes in an OAuth 2.0 protected resource metadata document.
pub const MAX_PROTECTED_RESOURCE_METADATA_BYTES: usize = 64 * 1024;
/// Maximum bytes in an OpenID Provider configuration document.
pub const MAX_OIDC_PROVIDER_METADATA_BYTES: usize = 128 * 1024;
/// Maximum bytes in a client-ID metadata document.
pub const MAX_CLIENT_ID_METADATA_BYTES: usize = 32 * 1024;
/// Maximum bytes in a dynamic client registration request or response.
pub const MAX_CLIENT_REGISTRATION_BYTES: usize = 32 * 1024;
/// Maximum bytes in a token endpoint response.
pub const MAX_TOKEN_RESPONSE_BYTES: usize = 16 * 1024;
/// Maximum bytes in a JWK Set document.
pub const MAX_JWK_SET_BYTES: usize = 64 * 1024;
/// Maximum bytes in a single JWK document.
pub const MAX_JWK_BYTES: usize = 8 * 1024;
/// Maximum bytes in a decoded compact-JWS protected header.
pub const MAX_JWS_PROTECTED_HEADER_BYTES: usize = 2 * 1024;
/// Maximum bytes in a decoded compact-JWS claims set.
pub const MAX_JWS_CLAIMS_BYTES: usize = 8 * 1024;
/// Maximum bytes in one encoded compact JWS.
pub const MAX_COMPACT_JWS_ENCODED_BYTES: usize = 16 * 1024;
/// Maximum bytes in a decoded compact-JWS signature.
pub const MAX_JWS_SIGNATURE_BYTES: usize = 1024;
/// Maximum bytes hashed for one RFC 7638 canonical thumbprint input.
pub const MAX_RFC7638_CANONICAL_INPUT_BYTES: usize = 2 * 1024;
/// Maximum bytes in an admitted `kid`.
pub const MAX_JWK_KID_BYTES: usize = 256;
/// Maximum keys admitted from one JWK Set.
pub const MAX_JWK_SET_KEYS: usize = 64;
/// Minimum admitted RSA modulus length in octets (2048-bit keys).
pub const MIN_RSA_MODULUS_BYTES: usize = 256;
/// Maximum admitted RSA modulus length in octets (4096-bit keys).
pub const MAX_RSA_MODULUS_BYTES: usize = 512;
/// The only admitted RSA public exponent, 65537, as minimal octets.
pub const ADMITTED_RSA_PUBLIC_EXPONENT: [u8; 3] = [0x01, 0x00, 0x01];

/// A stable reason security-bearing input was refused.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SecurityAdmissionError {
    /// The bounded raw JSON pass refused the document.
    Document(RawJsonAdmissionFailure),
    /// The compact serialization did not have exactly three segments.
    MalformedCompactSerialization,
    /// A compact segment was not canonical unpadded base64url within bounds.
    InvalidBase64Url(&'static str),
    /// The protected header or claims set was not a JSON object.
    NotAJsonObject(&'static str),
    /// The `typ` header did not match the selected closed profile.
    ProfileTypeMismatch,
    /// The `alg` header was absent, malformed, or not admitted.
    UnsupportedAlgorithm,
    /// A claim required by the selected profile was absent or mistyped.
    MissingOrMalformedClaim(&'static str),
    /// A claim forbidden by the selected profile was present.
    ForbiddenClaim(&'static str),
    /// The claim set matches a different closed profile's shape.
    CrossProfileConfusion,
    /// A JWK member was absent, mistyped, or not canonical.
    MalformedJwkMember(&'static str),
    /// The JWK carries private or symmetric key material.
    NonPublicKeyMaterial,
    /// The JWK is restricted to encryption or otherwise cannot verify.
    NotAVerificationKey,
    /// The JWK's `use` and `key_ops` members disagree.
    AmbiguousKeyUsage,
    /// The key is below the policy's minimum strength or above its maximum.
    UnsupportedKeyStrength,
    /// Two admitted keys in one set share a `kid`.
    DuplicateKeyIdentifier,
    /// A bounded input exceeded its fixed limit.
    InputTooLong(&'static str),
}

impl std::fmt::Display for SecurityAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Document(failure) => write!(formatter, "security document refused: {failure}"),
            Self::MalformedCompactSerialization => {
                formatter.write_str("compact serialization must have exactly three segments")
            }
            Self::InvalidBase64Url(part) => {
                write!(formatter, "{part} is not canonical unpadded base64url")
            }
            Self::NotAJsonObject(part) => write!(formatter, "{part} must be a JSON object"),
            Self::ProfileTypeMismatch => {
                formatter.write_str("the typ header does not match the selected JWS profile")
            }
            Self::UnsupportedAlgorithm => formatter.write_str("unsupported or absent alg header"),
            Self::MissingOrMalformedClaim(claim) => {
                write!(formatter, "claim {claim} is absent or malformed")
            }
            Self::ForbiddenClaim(claim) => {
                write!(formatter, "claim {claim} is forbidden by this profile")
            }
            Self::CrossProfileConfusion => {
                formatter.write_str("the claim set belongs to a different JWS profile")
            }
            Self::MalformedJwkMember(member) => {
                write!(formatter, "JWK member {member} is absent or not canonical")
            }
            Self::NonPublicKeyMaterial => {
                formatter.write_str("private or symmetric key material is never admitted")
            }
            Self::NotAVerificationKey => {
                formatter.write_str("the key is not admitted for signature verification")
            }
            Self::AmbiguousKeyUsage => formatter.write_str("use and key_ops disagree"),
            Self::UnsupportedKeyStrength => formatter.write_str("unsupported key strength"),
            Self::DuplicateKeyIdentifier => formatter.write_str("duplicate kid in one key set"),
            Self::InputTooLong(what) => write!(formatter, "{what} exceeds its fixed bound"),
        }
    }
}

impl std::error::Error for SecurityAdmissionError {}

impl From<RawJsonAdmissionFailure> for SecurityAdmissionError {
    fn from(failure: RawJsonAdmissionFailure) -> Self {
        Self::Document(failure)
    }
}

/// A security-bearing JSON document class and its fixed byte bound.
///
/// Naming the consumer in the type is what makes the shared semantics
/// checkable: every listed class reaches the same bounded pass, so no
/// security-bearing consumer can quietly acquire weaker admission than the
/// JSON-RPC envelope slice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SecurityDocumentKind {
    /// RFC 8414 authorization server metadata.
    AuthorizationServerMetadata,
    /// RFC 9728 protected resource metadata.
    ProtectedResourceMetadata,
    /// OpenID Provider configuration metadata.
    OidcProviderMetadata,
    /// A client-ID metadata document.
    ClientIdMetadataDocument,
    /// RFC 7591 dynamic client registration input or output.
    ClientRegistration,
    /// RFC 6749 token endpoint response.
    TokenResponse,
    /// RFC 7517 JWK Set.
    JsonWebKeySet,
    /// RFC 7517 single JWK.
    JsonWebKey,
    /// A decoded compact-JWS protected header.
    CompactJwsProtectedHeader,
    /// A decoded compact-JWS claims set.
    CompactJwsClaims,
}

impl SecurityDocumentKind {
    /// Returns the fixed byte bound this document class is admitted under.
    #[must_use]
    pub const fn byte_limit(self) -> usize {
        match self {
            Self::AuthorizationServerMetadata => MAX_OAUTH_METADATA_BYTES,
            Self::ProtectedResourceMetadata => MAX_PROTECTED_RESOURCE_METADATA_BYTES,
            Self::OidcProviderMetadata => MAX_OIDC_PROVIDER_METADATA_BYTES,
            Self::ClientIdMetadataDocument => MAX_CLIENT_ID_METADATA_BYTES,
            Self::ClientRegistration => MAX_CLIENT_REGISTRATION_BYTES,
            Self::TokenResponse => MAX_TOKEN_RESPONSE_BYTES,
            Self::JsonWebKeySet => MAX_JWK_SET_BYTES,
            Self::JsonWebKey => MAX_JWK_BYTES,
            Self::CompactJwsProtectedHeader => MAX_JWS_PROTECTED_HEADER_BYTES,
            Self::CompactJwsClaims => MAX_JWS_CLAIMS_BYTES,
        }
    }

    /// Every document class, so a consumer sweep cannot silently omit one.
    #[must_use]
    pub const fn all() -> [Self; 10] {
        [
            Self::AuthorizationServerMetadata,
            Self::ProtectedResourceMetadata,
            Self::OidcProviderMetadata,
            Self::ClientIdMetadataDocument,
            Self::ClientRegistration,
            Self::TokenResponse,
            Self::JsonWebKeySet,
            Self::JsonWebKey,
            Self::CompactJwsProtectedHeader,
            Self::CompactJwsClaims,
        ]
    }
}

/// Admits one security-bearing JSON document under its class bound.
///
/// This is the same bounded streaming pass the JSON-RPC envelope slice runs,
/// selected with the security-document top-level policy so a top-level array
/// is an ordinary shape violation rather than a JSON-RPC batch refusal.
pub fn admit_security_document(
    kind: SecurityDocumentKind,
    bytes: &[u8],
) -> Result<(), RawJsonAdmissionFailure> {
    crate::jsonrpc::admit_raw_json_document(
        bytes,
        kind.byte_limit(),
        RawJsonTopLevel::SecurityDocumentObject,
    )
}

/// Admits a security-bearing document and materializes its typed object.
///
/// Admission always precedes `serde_json`, so a duplicate member can never
/// reach the materialized map and two consumers of one document can never
/// disagree about which member they observed.
pub fn admit_security_document_object(
    kind: SecurityDocumentKind,
    bytes: &[u8],
) -> Result<Map<String, Value>, SecurityAdmissionError> {
    admit_security_document(kind, bytes)?;
    // Admission already proved one bounded top-level object with no duplicate
    // members, so this decode cannot observe a different shape or a
    // last-member-wins reading.
    serde_json::from_slice::<Map<String, Value>>(bytes)
        .map_err(|_| SecurityAdmissionError::NotAJsonObject("document"))
}

/// Admits one decoded compact-JWS part, naming it in a shape failure.
fn admit_jws_part(
    kind: SecurityDocumentKind,
    bytes: &[u8],
    part: &'static str,
) -> Result<Map<String, Value>, SecurityAdmissionError> {
    match admit_security_document_object(kind, bytes) {
        Err(SecurityAdmissionError::Document(failure))
            if failure.error() == RawJsonAdmissionError::TopLevelNotObject =>
        {
            Err(SecurityAdmissionError::NotAJsonObject(part))
        }
        other => other,
    }
}

/// The closed set of compact-JWS profiles this crate admits.
///
/// The profile is chosen by the local call site, never by a `typ` string taken
/// from the token. Each profile owns its header grammar and its required,
/// authorization-relevant, and forbidden claim semantics, so bytes admitted
/// for one profile cannot be reinterpreted under another.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CompactJwsProfile {
    /// RFC 9068 JWT access token, distinguished by its `at+jwt` media type.
    Rfc9068AccessToken,
    /// OpenID Connect ID Token, asserting an end user to a relying party.
    OidcIdToken,
    /// Identity Assertion JWT Authorization Grant.
    IdentityAssertionJwtAuthorizationGrant,
    /// RFC 7523 client assertion, where the client speaks for itself.
    Rfc7523ClientAssertion,
    /// A token this deployment's built-in issuer minted and now verifies.
    BuiltInIssuerSelfVerification,
}

impl CompactJwsProfile {
    /// Every profile, so an exhaustiveness sweep cannot omit one.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::Rfc9068AccessToken,
            Self::OidcIdToken,
            Self::IdentityAssertionJwtAuthorizationGrant,
            Self::Rfc7523ClientAssertion,
            Self::BuiltInIssuerSelfVerification,
        ]
    }

    /// Returns the canonical shortened lowercase `typ` this profile emits.
    ///
    /// Emission is always the shortened lowercase form even though admission
    /// accepts the `application/` prefix and ASCII case variants, per RFC 7515
    /// Section 4.1.9.
    #[must_use]
    pub const fn canonical_media_type(self) -> Option<&'static str> {
        match self {
            Self::Rfc9068AccessToken => Some("at+jwt"),
            Self::IdentityAssertionJwtAuthorizationGrant => Some("oauth-id-jag+jwt"),
            Self::OidcIdToken
            | Self::Rfc7523ClientAssertion
            | Self::BuiltInIssuerSelfVerification => Some("jwt"),
        }
    }

    /// Returns whether an absent `typ` header satisfies this profile.
    #[must_use]
    pub const fn permits_absent_media_type(self) -> bool {
        matches!(self, Self::OidcIdToken | Self::Rfc7523ClientAssertion)
    }

    /// Returns the claims this profile treats as authorization-relevant.
    ///
    /// Unknown claims outside this set are syntactically admitted and ignored.
    /// This is deliberately not a closed allowlist of claim names.
    #[must_use]
    pub const fn authorization_relevant_claims(self) -> &'static [&'static str] {
        match self {
            Self::Rfc9068AccessToken => &["scope", "groups", "roles", "entitlements", "client_id"],
            Self::OidcIdToken => &["nonce", "auth_time", "acr", "amr", "azp"],
            Self::IdentityAssertionJwtAuthorizationGrant => &["scope", "client_id", "azp"],
            Self::Rfc7523ClientAssertion => &["jti"],
            Self::BuiltInIssuerSelfVerification => &["scope", "jti"],
        }
    }

    /// Returns the claims whose presence this profile refuses outright.
    ///
    /// Each entry is a claim that carries authorization meaning under a
    /// different profile, so admitting it here would be exactly the
    /// cross-profile reinterpretation this closed enum exists to prevent.
    #[must_use]
    pub const fn forbidden_claims(self) -> &'static [&'static str] {
        match self {
            // `nonce` and `at_hash` bind an ID token to an authorization
            // request; an access token that carried them could be replayed
            // into an OIDC relying party.
            Self::Rfc9068AccessToken => &["nonce", "at_hash"],
            // `scope` and `client_id` are access-token authorization input; an
            // ID token must never supply them.
            Self::OidcIdToken => &["scope", "client_id"],
            Self::IdentityAssertionJwtAuthorizationGrant => &["nonce", "at_hash"],
            // A client assertion authenticates a client; it never carries user
            // authorization or ID-token binding.
            Self::Rfc7523ClientAssertion => &["scope", "nonce", "at_hash", "groups", "roles"],
            Self::BuiltInIssuerSelfVerification => &["nonce", "at_hash"],
        }
    }

    /// Returns the claims that must be present and well typed.
    #[must_use]
    pub const fn required_claims(self) -> &'static [&'static str] {
        match self {
            Self::Rfc9068AccessToken => &["iss", "sub", "aud", "exp", "iat", "jti"],
            Self::OidcIdToken => &["iss", "sub", "aud", "exp", "iat"],
            Self::IdentityAssertionJwtAuthorizationGrant => &["iss", "sub", "aud", "exp", "iat"],
            Self::Rfc7523ClientAssertion => &["iss", "sub", "aud", "exp", "jti"],
            Self::BuiltInIssuerSelfVerification => &["iss", "sub", "aud", "exp", "iat"],
        }
    }
}

/// The admitted protected header of one compact JWS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JwsProtectedHeader {
    algorithm: String,
    media_type: Option<String>,
    key_id: Option<String>,
}

impl JwsProtectedHeader {
    /// Returns the admitted `alg` value.
    #[must_use]
    pub fn algorithm(&self) -> &str {
        &self.algorithm
    }

    /// Returns the normalized shortened lowercase `typ`, when present.
    #[must_use]
    pub fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }

    /// Returns the admitted `kid`, when present.
    #[must_use]
    pub fn key_id(&self) -> Option<&str> {
        self.key_id.as_deref()
    }
}

/// One compact JWS admitted under a closed profile.
///
/// No signature has been verified. `signing_input` and `signature` are the
/// exact bytes a verifier needs; producing them is deliberately the end of
/// this module's responsibility.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedCompactJws {
    profile: CompactJwsProfile,
    header: JwsProtectedHeader,
    claims: Map<String, Value>,
    signing_input: String,
    signature: Vec<u8>,
}

impl AdmittedCompactJws {
    /// Returns the profile this token was admitted under.
    #[must_use]
    pub const fn profile(&self) -> CompactJwsProfile {
        self.profile
    }

    /// Returns the admitted protected header.
    #[must_use]
    pub const fn header(&self) -> &JwsProtectedHeader {
        &self.header
    }

    /// Returns the admitted claim set, including inert unknown claims.
    #[must_use]
    pub const fn claims(&self) -> &Map<String, Value> {
        &self.claims
    }

    /// Returns one claim only when the admitting profile names it
    /// authorization-relevant.
    ///
    /// An unknown or merely-syntactic claim is inert: it is retained in
    /// [`Self::claims`] but can never be read as authorization input through
    /// this accessor.
    #[must_use]
    pub fn authorization_claim(&self, name: &str) -> Option<&Value> {
        if self.profile.authorization_relevant_claims().contains(&name) {
            self.claims.get(name)
        } else {
            None
        }
    }

    /// Returns the exact `base64url(header).base64url(claims)` signing input.
    #[must_use]
    pub fn signing_input(&self) -> &str {
        &self.signing_input
    }

    /// Returns the decoded signature octets.
    #[must_use]
    pub fn signature(&self) -> &[u8] {
        &self.signature
    }
}

/// Normalizes a JOSE `typ` value to its shortened lowercase media type.
///
/// RFC 7515 Section 4.1.9 allows omitting a leading `application/`, and media
/// types compare case-insensitively, so `at+jwt`, `AT+JWT`, and
/// `application/at+JWT` are one semantic type. A parameter or any further
/// `/` is refused rather than normalized, so `at+jwt;charset=utf-8` and
/// `application/example/at+jwt` never enter an admitted variant.
fn normalize_jose_media_type(value: &str) -> Option<String> {
    if value.is_empty() || value.contains(';') || value.contains(char::is_whitespace) {
        return None;
    }
    let lowered = value.to_ascii_lowercase();
    let shortened = lowered.strip_prefix("application/").unwrap_or(&lowered);
    if shortened.is_empty() || shortened.contains('/') {
        return None;
    }
    Some(shortened.to_owned())
}

/// Admits one compact JWS under exactly one closed profile.
///
/// The `profile` argument comes from the local call site. Nothing in the token
/// selects it, so an attacker cannot promote bytes minted for one profile into
/// another by choosing a `typ`.
pub fn admit_compact_jws(
    profile: CompactJwsProfile,
    token: &str,
) -> Result<AdmittedCompactJws, SecurityAdmissionError> {
    if token.len() > MAX_COMPACT_JWS_ENCODED_BYTES {
        return Err(SecurityAdmissionError::InputTooLong("compact JWS"));
    }
    let mut segments = token.split('.');
    let (Some(header_segment), Some(claims_segment), Some(signature_segment), None) = (
        segments.next(),
        segments.next(),
        segments.next(),
        segments.next(),
    ) else {
        return Err(SecurityAdmissionError::MalformedCompactSerialization);
    };

    let header_bytes = decode_canonical_base64url(
        header_segment,
        MAX_JWS_PROTECTED_HEADER_BYTES,
        "protected header",
    )?;
    let claims_bytes =
        decode_canonical_base64url(claims_segment, MAX_JWS_CLAIMS_BYTES, "claims set")?;
    let signature =
        decode_canonical_base64url(signature_segment, MAX_JWS_SIGNATURE_BYTES, "signature")?;

    let header_members = admit_jws_part(
        SecurityDocumentKind::CompactJwsProtectedHeader,
        &header_bytes,
        "protected header",
    )?;
    let claims = admit_jws_part(
        SecurityDocumentKind::CompactJwsClaims,
        &claims_bytes,
        "claims set",
    )?;

    let header = admit_protected_header(profile, &header_members)?;
    admit_profile_claims(profile, &claims)?;

    Ok(AdmittedCompactJws {
        profile,
        header,
        claims,
        signing_input: format!("{header_segment}.{claims_segment}"),
        signature,
    })
}

fn admit_protected_header(
    profile: CompactJwsProfile,
    members: &Map<String, Value>,
) -> Result<JwsProtectedHeader, SecurityAdmissionError> {
    let Some(Value::String(algorithm)) = members.get("alg") else {
        return Err(SecurityAdmissionError::UnsupportedAlgorithm);
    };
    // `none` would make every signature check vacuous; it is refused here
    // rather than left for a verifier to remember.
    if algorithm.eq_ignore_ascii_case("none") || algorithm.is_empty() {
        return Err(SecurityAdmissionError::UnsupportedAlgorithm);
    }

    let media_type = match members.get("typ") {
        None => {
            if !profile.permits_absent_media_type() {
                return Err(SecurityAdmissionError::ProfileTypeMismatch);
            }
            None
        }
        Some(Value::String(raw)) => {
            let normalized = normalize_jose_media_type(raw)
                .ok_or(SecurityAdmissionError::ProfileTypeMismatch)?;
            if Some(normalized.as_str()) != profile.canonical_media_type() {
                return Err(SecurityAdmissionError::ProfileTypeMismatch);
            }
            Some(normalized)
        }
        Some(_) => return Err(SecurityAdmissionError::ProfileTypeMismatch),
    };

    let key_id = match members.get("kid") {
        None => None,
        Some(Value::String(kid)) if kid.len() <= MAX_JWK_KID_BYTES && !kid.is_empty() => {
            Some(kid.clone())
        }
        Some(_) => return Err(SecurityAdmissionError::MalformedJwkMember("kid")),
    };

    Ok(JwsProtectedHeader {
        algorithm: algorithm.clone(),
        media_type,
        key_id,
    })
}

fn claim_str<'a>(
    claims: &'a Map<String, Value>,
    name: &'static str,
) -> Result<&'a str, SecurityAdmissionError> {
    match claims.get(name) {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(SecurityAdmissionError::MissingOrMalformedClaim(name)),
    }
}

fn admit_profile_claims(
    profile: CompactJwsProfile,
    claims: &Map<String, Value>,
) -> Result<(), SecurityAdmissionError> {
    for required in profile.required_claims() {
        let Some(value) = claims.get(*required) else {
            return Err(SecurityAdmissionError::MissingOrMalformedClaim(required));
        };
        let well_typed = match *required {
            // NumericDate claims are JSON numbers, never strings.
            "exp" | "iat" | "nbf" => {
                matches!(value, Value::Number(number) if number.as_str().len() <= 32)
            }
            // `aud` is one non-empty string or a non-empty array of them.
            "aud" => match value {
                Value::String(entry) => !entry.is_empty(),
                Value::Array(entries) => {
                    !entries.is_empty()
                        && entries
                            .iter()
                            .all(|entry| matches!(entry, Value::String(text) if !text.is_empty()))
                }
                _ => false,
            },
            _ => matches!(value, Value::String(text) if !text.is_empty()),
        };
        if !well_typed {
            return Err(SecurityAdmissionError::MissingOrMalformedClaim(required));
        }
    }
    for forbidden in profile.forbidden_claims() {
        if claims.contains_key(*forbidden) {
            return Err(SecurityAdmissionError::ForbiddenClaim(forbidden));
        }
    }

    // `typ` alone cannot separate the three profiles that legitimately carry
    // `JWT`, so their issuer/subject/audience relationships do. These three
    // predicates are mutually exclusive by construction.
    let issuer = claim_str(claims, "iss")?;
    let subject = claim_str(claims, "sub")?;
    let self_issued = issuer == subject;
    let self_audienced = audience_contains(claims, issuer);
    match profile {
        CompactJwsProfile::OidcIdToken => {
            // An OP asserts an end user; it never asserts itself.
            if self_issued {
                return Err(SecurityAdmissionError::CrossProfileConfusion);
            }
        }
        CompactJwsProfile::Rfc7523ClientAssertion => {
            // RFC 7523 Section 3: for client authentication the issuer and the
            // subject are both the client, and the audience is the endpoint.
            if !self_issued || self_audienced {
                return Err(SecurityAdmissionError::CrossProfileConfusion);
            }
        }
        CompactJwsProfile::BuiltInIssuerSelfVerification => {
            // The built-in issuer minted this for itself.
            if !self_issued || !self_audienced {
                return Err(SecurityAdmissionError::CrossProfileConfusion);
            }
        }
        CompactJwsProfile::Rfc9068AccessToken
        | CompactJwsProfile::IdentityAssertionJwtAuthorizationGrant => {
            // These two are already separated by a mandatory media type.
        }
    }
    Ok(())
}

fn audience_contains(claims: &Map<String, Value>, candidate: &str) -> bool {
    match claims.get("aud") {
        Some(Value::String(value)) => value == candidate,
        Some(Value::Array(values)) => values
            .iter()
            .any(|value| matches!(value, Value::String(entry) if entry == candidate)),
        _ => false,
    }
}

/// Bounded, profile-parameterized admission policy for public JWK material.
///
/// The policy validates key type, algorithm, canonical public parameters,
/// minimum strength, `kid` uniqueness, `use`, and `key_ops`. It performs no
/// discovery, no key lookup, no signing, and no verification.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JwkAdmissionPolicy {
    minimum_rsa_modulus_bytes: usize,
    maximum_rsa_modulus_bytes: usize,
    require_key_id: bool,
}

impl Default for JwkAdmissionPolicy {
    fn default() -> Self {
        Self {
            minimum_rsa_modulus_bytes: MIN_RSA_MODULUS_BYTES,
            maximum_rsa_modulus_bytes: MAX_RSA_MODULUS_BYTES,
            require_key_id: true,
        }
    }
}

impl JwkAdmissionPolicy {
    /// Returns the policy's minimum admitted RSA modulus length in octets.
    #[must_use]
    pub const fn minimum_rsa_modulus_bytes(self) -> usize {
        self.minimum_rsa_modulus_bytes
    }

    /// Returns a policy that does not require a `kid`.
    ///
    /// A single-key deployment can address its key without one; a key set
    /// cannot, which is why [`admit_public_jwk_set`] still enforces `kid`
    /// uniqueness over whatever identifiers are present.
    #[must_use]
    pub const fn without_required_key_id(mut self) -> Self {
        self.require_key_id = false;
        self
    }

    /// Raises the minimum admitted RSA modulus length.
    ///
    /// The bound only ever moves up: a deployment may demand stronger keys
    /// than the compiled floor but can never talk this policy below it.
    #[must_use]
    pub const fn with_minimum_rsa_modulus_bytes(mut self, bytes: usize) -> Self {
        if bytes > self.minimum_rsa_modulus_bytes {
            self.minimum_rsa_modulus_bytes = bytes;
        }
        self
    }
}

/// Every JWK member that carries private or symmetric key material.
const PRIVATE_OR_SYMMETRIC_MEMBERS: [&str; 8] = ["d", "p", "q", "dp", "dq", "qi", "oth", "k"];

/// An admitted RSA public key in canonical form.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AdmittedRsaPublicJwk {
    modulus: Vec<u8>,
    exponent: Vec<u8>,
    key_id: Option<String>,
    algorithm: Option<String>,
}

impl AdmittedRsaPublicJwk {
    /// Returns the canonical minimal big-endian modulus octets.
    #[must_use]
    pub fn modulus(&self) -> &[u8] {
        &self.modulus
    }

    /// Returns the canonical minimal big-endian public exponent octets.
    #[must_use]
    pub fn exponent(&self) -> &[u8] {
        &self.exponent
    }

    /// Returns the admitted `kid`, when present.
    #[must_use]
    pub fn key_id(&self) -> Option<&str> {
        self.key_id.as_deref()
    }

    /// Returns the admitted `alg`, when present.
    #[must_use]
    pub fn algorithm(&self) -> Option<&str> {
        self.algorithm.as_deref()
    }

    /// Computes this key's RFC 7638 SHA-256 thumbprint.
    ///
    /// The canonical input is the UTF-8 bytes of
    /// `{"e":"<e>","kty":"RSA","n":"<n>"}` with the required members in
    /// lexicographic order and no whitespace, where `e` and `n` are canonical
    /// unpadded Base64url of the minimal big-endian octets. The bytes are
    /// hashed through the core `sha256_bounded` primitive under
    /// [`MAX_RFC7638_CANONICAL_INPUT_BYTES`]; this crate has no direct `sha2`
    /// edge.
    pub fn thumbprint_sha256(&self) -> Result<JwkThumbprintSha256, SecurityAdmissionError> {
        let canonical = self.rfc7638_canonical_input();
        if canonical.len() > MAX_RFC7638_CANONICAL_INPUT_BYTES {
            return Err(SecurityAdmissionError::InputTooLong(
                "RFC 7638 canonical input",
            ));
        }
        let digest =
            fastmcp_core::sha256_bounded(canonical.as_bytes(), MAX_RFC7638_CANONICAL_INPUT_BYTES)
                .map_err(|_| SecurityAdmissionError::InputTooLong("RFC 7638 canonical input"))?;
        Ok(JwkThumbprintSha256(*digest.as_bytes()))
    }

    /// Returns the exact RFC 7638 canonical JSON hashed for the thumbprint.
    ///
    /// It is public because an operator comparing thumbprints across systems
    /// needs to see the bytes that were hashed, not infer them.
    #[must_use]
    pub fn rfc7638_canonical_input(&self) -> String {
        format!(
            r#"{{"e":"{}","kty":"RSA","n":"{}"}}"#,
            encode_base64url(&self.exponent),
            encode_base64url(&self.modulus),
        )
    }
}

/// An RFC 7638 SHA-256 JWK thumbprint.
///
/// A JSON serialization, PEM, DER, KMS handle, display form, or digest of a
/// whole JWK is never this value. Any of those inputs must first normalize to
/// the same admitted public `(n, e)`.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct JwkThumbprintSha256([u8; 32]);

impl JwkThumbprintSha256 {
    /// Returns the raw 32-byte digest.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// Returns the canonical unpadded Base64url encoding.
    #[must_use]
    pub fn to_base64url(&self) -> String {
        encode_base64url(&self.0)
    }
}

impl std::fmt::Display for JwkThumbprintSha256 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.to_base64url())
    }
}

/// Admits one public RSA JWK from its already-materialized members.
pub fn admit_public_rsa_jwk(
    policy: JwkAdmissionPolicy,
    members: &Map<String, Value>,
) -> Result<AdmittedRsaPublicJwk, SecurityAdmissionError> {
    for private in PRIVATE_OR_SYMMETRIC_MEMBERS {
        if members.contains_key(private) {
            return Err(SecurityAdmissionError::NonPublicKeyMaterial);
        }
    }
    match members.get("kty") {
        Some(Value::String(kty)) if kty == "RSA" => {}
        // `oct` is symmetric material; anything else is not an admitted
        // verification key type here.
        Some(Value::String(kty)) if kty == "oct" => {
            return Err(SecurityAdmissionError::NonPublicKeyMaterial);
        }
        _ => return Err(SecurityAdmissionError::MalformedJwkMember("kty")),
    }

    let declared_use = match members.get("use") {
        None => None,
        Some(Value::String(value)) if value == "sig" => Some("sig"),
        Some(Value::String(value)) if value == "enc" => {
            return Err(SecurityAdmissionError::NotAVerificationKey);
        }
        _ => return Err(SecurityAdmissionError::MalformedJwkMember("use")),
    };

    let declared_ops = match members.get("key_ops") {
        None => None,
        Some(Value::Array(values)) => {
            let mut operations = BTreeSet::new();
            for value in values {
                let Value::String(operation) = value else {
                    return Err(SecurityAdmissionError::MalformedJwkMember("key_ops"));
                };
                // Duplicate operations are explicitly forbidden by RFC 7517.
                if !operations.insert(operation.as_str()) {
                    return Err(SecurityAdmissionError::MalformedJwkMember("key_ops"));
                }
            }
            for private in ["sign", "decrypt", "unwrapKey", "deriveKey", "deriveBits"] {
                if operations.contains(private) {
                    return Err(SecurityAdmissionError::NonPublicKeyMaterial);
                }
            }
            if !operations.contains("verify") {
                return Err(SecurityAdmissionError::NotAVerificationKey);
            }
            Some(operations)
        }
        _ => return Err(SecurityAdmissionError::MalformedJwkMember("key_ops")),
    };

    // A key that says `use: sig` while withholding `verify`, or that declares
    // encryption operations beside a signing use, is ambiguous rather than
    // merely wrong, and is refused without guessing an intent.
    if let (Some(_), Some(operations)) = (declared_use, declared_ops.as_ref()) {
        if operations.contains("encrypt") || operations.contains("wrapKey") {
            return Err(SecurityAdmissionError::AmbiguousKeyUsage);
        }
    }

    let modulus = decode_base64url_uint(members.get("n"), MAX_RSA_MODULUS_BYTES, "n")?;
    let exponent = decode_base64url_uint(members.get("e"), 8, "e")?;
    if modulus.len() < policy.minimum_rsa_modulus_bytes
        || modulus.len() > policy.maximum_rsa_modulus_bytes
    {
        return Err(SecurityAdmissionError::UnsupportedKeyStrength);
    }
    if exponent.as_slice() != ADMITTED_RSA_PUBLIC_EXPONENT {
        return Err(SecurityAdmissionError::UnsupportedKeyStrength);
    }

    let key_id = match members.get("kid") {
        None if policy.require_key_id => {
            return Err(SecurityAdmissionError::MalformedJwkMember("kid"));
        }
        None => None,
        Some(Value::String(kid)) if !kid.is_empty() && kid.len() <= MAX_JWK_KID_BYTES => {
            Some(kid.clone())
        }
        Some(_) => return Err(SecurityAdmissionError::MalformedJwkMember("kid")),
    };

    let algorithm = match members.get("alg") {
        None => None,
        Some(Value::String(alg)) if alg.starts_with("RS") || alg.starts_with("PS") => {
            Some(alg.clone())
        }
        _ => return Err(SecurityAdmissionError::UnsupportedAlgorithm),
    };

    Ok(AdmittedRsaPublicJwk {
        modulus,
        exponent,
        key_id,
        algorithm,
    })
}

/// Admits a whole JWK Set document, enforcing `kid` uniqueness across it.
pub fn admit_public_jwk_set(
    policy: JwkAdmissionPolicy,
    bytes: &[u8],
) -> Result<Vec<AdmittedRsaPublicJwk>, SecurityAdmissionError> {
    let document = admit_security_document_object(SecurityDocumentKind::JsonWebKeySet, bytes)?;
    let Some(Value::Array(entries)) = document.get("keys") else {
        return Err(SecurityAdmissionError::MalformedJwkMember("keys"));
    };
    if entries.len() > MAX_JWK_SET_KEYS {
        return Err(SecurityAdmissionError::InputTooLong("JWK set"));
    }

    let mut admitted = Vec::with_capacity(entries.len());
    let mut identifiers = BTreeSet::new();
    for entry in entries {
        let Value::Object(members) = entry else {
            return Err(SecurityAdmissionError::MalformedJwkMember("keys"));
        };
        let key = admit_public_rsa_jwk(policy, members)?;
        if let Some(kid) = key.key_id() {
            if !identifiers.insert(kid.to_owned()) {
                return Err(SecurityAdmissionError::DuplicateKeyIdentifier);
            }
        }
        admitted.push(key);
    }
    Ok(admitted)
}

/// Normalizes an already-parsed public `(modulus, exponent)` pair.
///
/// A KMS handle, an SPKI/DER export, and a public JWK all reach the same
/// admitted value through this entry point, so none of them can produce a
/// thumbprint the others would not.
pub fn admit_public_rsa_components(
    policy: JwkAdmissionPolicy,
    modulus: &[u8],
    exponent: &[u8],
    key_id: Option<&str>,
) -> Result<AdmittedRsaPublicJwk, SecurityAdmissionError> {
    let Some(modulus) = minimal_unsigned_octets(modulus) else {
        return Err(SecurityAdmissionError::MalformedJwkMember("n"));
    };
    let Some(exponent) = minimal_unsigned_octets(exponent) else {
        return Err(SecurityAdmissionError::MalformedJwkMember("e"));
    };
    if modulus.len() < policy.minimum_rsa_modulus_bytes
        || modulus.len() > policy.maximum_rsa_modulus_bytes
    {
        return Err(SecurityAdmissionError::UnsupportedKeyStrength);
    }
    if exponent.as_slice() != ADMITTED_RSA_PUBLIC_EXPONENT {
        return Err(SecurityAdmissionError::UnsupportedKeyStrength);
    }
    let key_id = match key_id {
        None if policy.require_key_id => {
            return Err(SecurityAdmissionError::MalformedJwkMember("kid"));
        }
        None => None,
        Some(kid) if !kid.is_empty() && kid.len() <= MAX_JWK_KID_BYTES => Some(kid.to_owned()),
        Some(_) => return Err(SecurityAdmissionError::MalformedJwkMember("kid")),
    };
    Ok(AdmittedRsaPublicJwk {
        modulus,
        exponent,
        key_id,
        algorithm: None,
    })
}

/// Strips leading zero octets, refusing an all-zero or empty integer.
///
/// RFC 7518 Base64urlUInt octets are the minimum-length big-endian
/// representation, so a stored value with a redundant leading zero is not
/// canonical. A DER or KMS export commonly carries one; normalizing here is
/// what lets those inputs converge on one thumbprint.
fn minimal_unsigned_octets(bytes: &[u8]) -> Option<Vec<u8>> {
    let first_significant = bytes.iter().position(|octet| *octet != 0)?;
    Some(bytes[first_significant..].to_vec())
}

fn decode_base64url_uint(
    value: Option<&Value>,
    max_bytes: usize,
    member: &'static str,
) -> Result<Vec<u8>, SecurityAdmissionError> {
    let Some(Value::String(encoded)) = value else {
        return Err(SecurityAdmissionError::MalformedJwkMember(member));
    };
    let decoded = decode_canonical_base64url(encoded, max_bytes, member)
        .map_err(|_| SecurityAdmissionError::MalformedJwkMember(member))?;
    // A Base64urlUInt is nonempty and carries no redundant leading zero.
    if decoded.is_empty() || decoded[0] == 0 {
        return Err(SecurityAdmissionError::MalformedJwkMember(member));
    }
    Ok(decoded)
}

fn encode_base64url(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Decodes canonical unpadded base64url within a fixed decoded-byte bound.
///
/// The re-encode comparison rejects non-canonical spellings — padding,
/// standard-alphabet characters, and trailing bits that do not round-trip —
/// so one encoded form maps to one decoded value and back.
fn decode_canonical_base64url(
    encoded: &str,
    max_decoded_bytes: usize,
    part: &'static str,
) -> Result<Vec<u8>, SecurityAdmissionError> {
    let max_encoded = max_decoded_bytes.div_ceil(3).saturating_mul(4);
    if encoded.is_empty() || encoded.len() > max_encoded {
        return Err(SecurityAdmissionError::InvalidBase64Url(part));
    }
    if !encoded
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
    {
        return Err(SecurityAdmissionError::InvalidBase64Url(part));
    }
    let decoded = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| SecurityAdmissionError::InvalidBase64Url(part))?;
    if decoded.len() > max_decoded_bytes {
        return Err(SecurityAdmissionError::InputTooLong(part));
    }
    if encode_base64url(&decoded) != encoded {
        return Err(SecurityAdmissionError::InvalidBase64Url(part));
    }
    Ok(decoded)
}
