//! FND-01 surface 2: an external-consumer mirror of `oidc::non_signing_tests`.
//!
//! # This target mirrors, it does not replace
//!
//! The frozen `PL-1-TARGET-MAP-V1` in `bd-fnd-01-implementation-b-zz0r` pins
//! surface 2 to the **`fastmcp-server` lib target** under
//! `oidc::non_signing_tests`, and that same map defines case identity as the
//! tuple *(Cargo manifest/package, target kind, target name,
//! cfg/profile/features/platform, fully qualified case ID)*. Relocating those
//! cases into a test target would change `target kind` from `lib` to `test`,
//! which by the map's own rule makes them a **different surface** — so a
//! relocation would break the frozen map it was meant to satisfy.
//!
//! The inline `#[cfg(test)] mod non_signing_tests` is therefore retained
//! deliberately and must not be moved or deleted. This file is an addition
//! beside it: the same behaviours, proved through the packaged public surface
//! `fastmcp_server::{oauth, oidc}` exactly as a downstream consumer reaches
//! them, with no `use super::`, no `pub(crate)` widening, and no access to
//! crate internals. Together the two targets satisfy both the frozen map and
//! PL-3; neither alone does.
//!
//! # Coverage: six of the seven frozen cases
//!
//! | frozen case | mirrored here |
//! |---|---|
//! | `default_oidc_and_oauth_issuers_match` | yes |
//! | `provider_requires_exact_safe_oauth_issuer_and_defaults_follow_custom_oauth` | yes |
//! | `discovery_does_not_advertise_signing` | yes |
//! | `issuance_fails_closed_without_external_signer` | yes |
//! | `user_claims_filter_by_scope` | yes |
//! | `claims_provider_cannot_substitute_a_different_subject` | **no — see below** |
//! | `oidc_debug_surfaces_redact_token_and_pii_canaries_without_changing_wire_data` | yes |
//!
//! `claims_provider_cannot_substitute_a_different_subject` is **not** mirrored.
//! It observes the refusal through `OidcProvider::get_user_claims`, which is
//! private (`crates/fastmcp-server/src/oidc.rs:2184`, `fn get_user_claims`,
//! no `pub`). There is currently no public observation path for that
//! security-relevant refusal, and widening a production API purely to host a
//! test is the mirror image of the `cfg(test)` defect this addition exists to
//! correct. That gap is recorded as a finding on the owning bead rather than
//! papered over here, and the case remains proved on the lib target only.
//!
//! Every case name below is identical to its lib-target counterpart. The names
//! are deliberately not uniquified: the target kind already distinguishes the
//! two surfaces, and matching names make the mirror relationship legible.

use std::sync::Arc;

use fastmcp_server::oauth::{OAuthError, OAuthServer, OAuthServerConfig};
use fastmcp_server::oidc::{
    AddressClaim, DiscoveryDocument, IdToken, IdTokenClaims, InMemoryClaimsProvider, OidcError,
    OidcProvider, OidcProviderConfig, UserClaims,
};

#[test]
fn default_oidc_and_oauth_issuers_match() {
    assert_eq!(
        OidcProviderConfig::default().issuer,
        OAuthServerConfig::default().issuer
    );
}

#[test]
fn provider_requires_exact_safe_oauth_issuer_and_defaults_follow_custom_oauth() {
    let oauth = Arc::new(
        OAuthServer::try_new(OAuthServerConfig {
            issuer: "https://issuer.example/tenant".to_string(),
            ..OAuthServerConfig::default()
        })
        .expect("a safe HTTPS issuer must construct"),
    );
    let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("default provider");
    assert_eq!(provider.config().issuer, oauth.config().issuer);
    assert_eq!(
        provider.discovery_document("https://issuer.example").issuer,
        oauth.config().issuer
    );

    // A provider whose configured issuer does not match its OAuth server is
    // refused rather than silently reconciled.
    assert!(matches!(
        OidcProvider::new(Arc::clone(&oauth), OidcProviderConfig::default()),
        Err(OidcError::OAuth(OAuthError::ServerError(_)))
    ));

    // The sole changed field is the issuer scheme: https becomes cleartext.
    let unsafe_config = OidcProviderConfig {
        issuer: "http://issuer.example".to_string(),
        ..OidcProviderConfig::default()
    };
    assert!(matches!(
        OidcProvider::new(oauth, unsafe_config),
        Err(OidcError::OAuth(OAuthError::ServerError(_)))
    ));
}

#[test]
fn discovery_does_not_advertise_signing() {
    let doc = DiscoveryDocument::new("https://issuer.example", "https://issuer.example");
    assert_eq!(doc.id_token_signing_alg_values_supported.len(), 0);
    assert!(doc.jwks_uri.is_none());
    assert_eq!(
        doc.code_challenge_methods_supported,
        Some(vec!["S256".to_string()])
    );
}

/// Mirrors the ungated half of frozen case 4.
///
/// **This proves the absence of an advertised issuance path, not the runtime
/// refusal.** The security claim — that a configured issuance call refuses
/// when no external signer is activated — requires the `builtin-auth-server`
/// feature, under which `OidcProvider::issue_id_token` exists at all. It is
/// proved by `oidc::signer_activation_tests::
/// issuance_without_activation_fails_closed_with_signing_error` on the lib
/// target, and is deliberately not restated here, because this target builds
/// under the crate's default features where that entry point does not exist.
///
/// Distinct from `discovery_does_not_advertise_signing` above, which asserts
/// the defaults of a bare `DiscoveryDocument`. This asserts what a live
/// provider actually advertises to a relying party.
#[test]
fn issuance_fails_closed_without_external_signer() {
    let oauth = Arc::new(OAuthServer::new(OAuthServerConfig::default()));
    let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("default provider");

    let discovery = provider.discovery_document("https://issuer.example");

    assert!(
        discovery.id_token_signing_alg_values_supported.is_empty(),
        "a provider with no activated external signer must advertise no \
         ID-token signing algorithm"
    );
    assert!(
        discovery.jwks_uri.is_none(),
        "a provider with no activated external signer must advertise no \
         JWKS endpoint"
    );
}

#[test]
fn user_claims_filter_by_scope() {
    let claims = UserClaims::new("subject")
        .with_name("Alice")
        .with_email("alice@example.test")
        .with_email_verified(true);
    let filtered = claims.filter_by_scopes(&["openid".to_string()]);
    assert_eq!(filtered.sub, "subject");
    assert!(filtered.name.is_none());
    assert!(filtered.email.is_none());
}

#[test]
fn oidc_debug_surfaces_redact_token_and_pii_canaries_without_changing_wire_data() {
    const CANARY: &str = "oidc-debug-token-pii-canary";
    let address = AddressClaim {
        formatted: Some(CANARY.to_owned()),
        street_address: Some(CANARY.to_owned()),
        locality: Some(CANARY.to_owned()),
        region: Some(CANARY.to_owned()),
        postal_code: Some(CANARY.to_owned()),
        country: Some(CANARY.to_owned()),
    };
    let mut user_claims = UserClaims::new(CANARY)
        .with_name(CANARY)
        .with_email(CANARY)
        .with_email_verified(true)
        .with_phone_number(CANARY)
        .with_custom(CANARY, serde_json::json!(CANARY));
    user_claims.preferred_username = Some(CANARY.to_owned());
    user_claims.address = Some(address.clone());
    let id_token_claims = IdTokenClaims {
        iss: CANARY.to_owned(),
        sub: CANARY.to_owned(),
        aud: CANARY.to_owned(),
        exp: 2,
        iat: 1,
        auth_time: Some(1),
        nonce: Some(CANARY.to_owned()),
        acr: Some(CANARY.to_owned()),
        amr: Some(vec![CANARY.to_owned()]),
        azp: Some(CANARY.to_owned()),
        at_hash: Some(CANARY.to_owned()),
        c_hash: Some(CANARY.to_owned()),
        user_claims: user_claims.clone(),
    };

    // Redaction must not alter the wire encoding: the serialized claims still
    // carry every value verbatim.
    let wire = serde_json::to_value(&id_token_claims).expect("claims serialize");
    assert_eq!(wire["nonce"], CANARY);
    assert_eq!(wire["email"], CANARY);
    assert_eq!(wire["phone_number"], CANARY);
    assert_eq!(wire["address"]["formatted"], CANARY);
    assert_eq!(wire[CANARY], CANARY);

    let id_token = IdToken {
        raw: CANARY.to_owned(),
        claims: id_token_claims.clone(),
    };
    let provider = InMemoryClaimsProvider::new();
    provider.set_claims(user_claims.clone());
    let errors = [
        OidcError::ClaimsNotFound(CANARY.to_owned()),
        OidcError::SigningError(CANARY.to_owned()),
        OidcError::InvalidIdToken(CANARY.to_owned()),
    ];
    let debug_outputs = [
        format!("{address:?}"),
        format!("{user_claims:?}"),
        format!("{id_token_claims:?}"),
        format!("{id_token:?}"),
        format!("{provider:?}"),
        format!("{:?}", errors[0]),
        format!("{:?}", errors[1]),
        format!("{:?}", errors[2]),
    ];

    for debug in debug_outputs {
        assert!(
            !debug.contains(CANARY),
            "sensitive canary leaked through Debug: {debug}"
        );
        assert!(
            debug.contains("_len") || debug.contains("_count") || debug.contains("_present"),
            "Debug output lacked safe structural metadata: {debug}"
        );
    }

    for display in errors.map(|error| error.to_string()) {
        assert!(
            !display.contains(CANARY),
            "sensitive canary leaked through Display: {display}"
        );
    }
}
