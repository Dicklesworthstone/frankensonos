//! OpenID Connect (OIDC) Provider for MCP.
//!
//! This module extends the OAuth 2.0/2.1 server with OpenID Connect identity
//! layer features:
//!
//! - **ID Token Issuance**: fail-closed until an FND-09 external signer is admitted
//! - **UserInfo Endpoint**: Standard endpoint for retrieving user claims
//! - **Discovery Document**: `.well-known/openid-configuration` metadata
//! - **Standard Claims**: OpenID Connect standard claim types
//!
//! This source inventory is not a promoted production OIDC profile. AUTH and
//! MCP 2026-07-28 conformance gates remain outstanding; the public protocol
//! constant is still `2024-11-05`.
//!
//! # Architecture
//!
//! The OIDC provider builds on top of [`OAuthServer`] by:
//!
//! 1. Adding the `openid` scope to enable OIDC flows
//! 2. Reserving ID-token issuance for an FND-09 externally custodied signer
//! 3. Providing standard endpoints for identity operations
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Arc;
//!
//! use fastmcp_rust::oidc::{OidcProvider, OidcProviderConfig, UserClaims};
//! use fastmcp_rust::oauth::{OAuthServer, OAuthServerConfig};
//!
//! // Create OAuth server first
//! let oauth = Arc::new(OAuthServer::new(OAuthServerConfig::default()));
//!
//! // Create OIDC provider on top
//! let oidc = OidcProvider::new(oauth, OidcProviderConfig::default())
//!     .expect("oidc provider");
//!
//! // Set up user claims provider
//! oidc.set_claims_fn(|subject| {
//!     Some(UserClaims::new(subject)
//!         .with_name("John Doe")
//!         .with_email("john@example.com"))
//! });
//! ```

#[cfg(feature = "builtin-auth-server")]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(feature = "builtin-auth-server")]
use std::future::Future;
#[cfg(feature = "builtin-auth-server")]
use std::pin::Pin;
use std::sync::{Arc, RwLock};
#[cfg(feature = "builtin-auth-server")]
use std::time::{Instant, SystemTime, UNIX_EPOCH};

#[cfg(feature = "builtin-auth-server")]
use fastmcp_core::Cx;
#[cfg(feature = "builtin-auth-server")]
use fastmcp_protocol::jose::{
    AdmittedRsaJwks, BoundedJwsClaims, CanonicalRs256PublicJwks, CanonicalRs256PublicJwksSet,
    ExternalRs256Signer, ExternalRs256SigningDeadline, JwksEndpointReadBack, JwsSigningError,
    JwsSigningProfile, MAX_JWKS_BYTES, MAX_JWKS_KEYS, MAX_KID_BYTES, Rs256PublicKeyRing,
    Rs256SigningBinding, SigningActivationProfile, SigningActivationReceipt,
    verify_compact_jws_rs256,
};
#[cfg(feature = "builtin-auth-server")]
use url::Url;

use crate::oauth::{OAuthError, OAuthServer, OAuthServerConfig, OAuthToken, validate_oauth_issuer};

#[cfg(feature = "builtin-auth-server")]
const MAX_OIDC_NONCE_BYTES: usize = 1_024;

/// Fixed framework-owned claims for the external signer health canary.
///
/// This is not an ID token and is never exposed to a client. Its issuer,
/// endpoint, canonical public bytes, and signer generations are instead bound
/// by the sealed activation state that owns the canary result. Keeping this
/// input fixed lets a KMS/HSM integration provision one independently audited
/// test vector without putting any private material in this process.
#[cfg(feature = "builtin-auth-server")]
const OIDC_SIGNING_CANARY_CLAIMS: &str = r#"{"sub":"fixed-vector","aud":"server-policy-later"}"#;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the OIDC provider.
#[derive(Debug, Clone)]
pub struct OidcProviderConfig {
    /// Issuer identifier (URL) - must match OAuth server issuer.
    pub issuer: String,
    /// Supported claims.
    pub supported_claims: Vec<String>,
    /// Supported scopes beyond `openid`.
    pub supported_scopes: Vec<String>,
}

impl Default for OidcProviderConfig {
    fn default() -> Self {
        Self {
            issuer: OAuthServerConfig::default().issuer,
            supported_claims: vec![
                "sub".to_string(),
                "name".to_string(),
                "email".to_string(),
                "email_verified".to_string(),
                "preferred_username".to_string(),
                "picture".to_string(),
                "updated_at".to_string(),
            ],
            supported_scopes: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
        }
    }
}

// =============================================================================
// User Claims
// =============================================================================

/// Standard OpenID Connect user claims.
///
/// These claims describe the authenticated user and are included in
/// ID tokens and returned from the userinfo endpoint.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UserClaims {
    /// Subject identifier (required, unique user ID).
    pub sub: String,

    // Profile scope claims
    /// User's full name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// User's given/first name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub given_name: Option<String>,
    /// User's family/last name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub family_name: Option<String>,
    /// User's middle name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub middle_name: Option<String>,
    /// User's nickname/username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nickname: Option<String>,
    /// User's preferred username.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub preferred_username: Option<String>,
    /// URL of user's profile page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// URL of user's profile picture.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub picture: Option<String>,
    /// URL of user's website.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub website: Option<String>,
    /// User's gender.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gender: Option<String>,
    /// User's birthday (ISO 8601 date).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub birthdate: Option<String>,
    /// User's timezone (IANA timezone string).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zoneinfo: Option<String>,
    /// User's locale (BCP47 language tag).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locale: Option<String>,
    /// Time the user's info was last updated (Unix timestamp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<i64>,

    // Email scope claims
    /// User's email address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Whether the email has been verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub email_verified: Option<bool>,

    // Phone scope claims
    /// User's phone number.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone_number: Option<String>,
    /// Whether the phone number has been verified.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phone_number_verified: Option<bool>,

    // Address scope claims
    /// User's address (JSON object).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<AddressClaim>,

    /// Additional custom claims.
    #[serde(flatten)]
    pub custom: HashMap<String, serde_json::Value>,
}

impl std::fmt::Debug for UserClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let profile_claim_count = [
            self.name.is_some(),
            self.given_name.is_some(),
            self.family_name.is_some(),
            self.middle_name.is_some(),
            self.nickname.is_some(),
            self.preferred_username.is_some(),
            self.profile.is_some(),
            self.picture.is_some(),
            self.website.is_some(),
            self.gender.is_some(),
            self.birthdate.is_some(),
            self.zoneinfo.is_some(),
            self.locale.is_some(),
            self.updated_at.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();

        f.debug_struct("UserClaims")
            .field("subject_len", &self.sub.len())
            .field("profile_claim_count", &profile_claim_count)
            .field("email_present", &self.email.is_some())
            .field("email_verified_present", &self.email_verified.is_some())
            .field("phone_number_present", &self.phone_number.is_some())
            .field(
                "phone_number_verified_present",
                &self.phone_number_verified.is_some(),
            )
            .field("address_present", &self.address.is_some())
            .field("custom_claim_count", &self.custom.len())
            .finish()
    }
}

impl UserClaims {
    /// Creates new user claims with the given subject.
    #[must_use]
    pub fn new(sub: impl Into<String>) -> Self {
        Self {
            sub: sub.into(),
            ..Default::default()
        }
    }

    /// Sets the user's full name.
    #[must_use]
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the user's email.
    #[must_use]
    pub fn with_email(mut self, email: impl Into<String>) -> Self {
        self.email = Some(email.into());
        self
    }

    /// Sets whether the email is verified.
    #[must_use]
    pub fn with_email_verified(mut self, verified: bool) -> Self {
        self.email_verified = Some(verified);
        self
    }

    /// Sets the user's preferred username.
    #[must_use]
    pub fn with_preferred_username(mut self, username: impl Into<String>) -> Self {
        self.preferred_username = Some(username.into());
        self
    }

    /// Sets the user's profile picture URL.
    #[must_use]
    pub fn with_picture(mut self, url: impl Into<String>) -> Self {
        self.picture = Some(url.into());
        self
    }

    /// Sets the user's given name.
    #[must_use]
    pub fn with_given_name(mut self, name: impl Into<String>) -> Self {
        self.given_name = Some(name.into());
        self
    }

    /// Sets the user's family name.
    #[must_use]
    pub fn with_family_name(mut self, name: impl Into<String>) -> Self {
        self.family_name = Some(name.into());
        self
    }

    /// Sets the user's phone number.
    #[must_use]
    pub fn with_phone_number(mut self, phone: impl Into<String>) -> Self {
        self.phone_number = Some(phone.into());
        self
    }

    /// Sets the updated_at timestamp.
    #[must_use]
    pub fn with_updated_at(mut self, timestamp: i64) -> Self {
        self.updated_at = Some(timestamp);
        self
    }

    /// Adds a custom claim.
    #[must_use]
    pub fn with_custom(mut self, key: impl Into<String>, value: serde_json::Value) -> Self {
        self.custom.insert(key.into(), value);
        self
    }

    /// Filters claims based on requested scopes.
    ///
    /// Only returns claims that are allowed by the given scopes.
    #[must_use]
    #[allow(clippy::assigning_clones)]
    pub fn filter_by_scopes(&self, scopes: &[String]) -> UserClaims {
        let mut filtered = UserClaims::new(&self.sub);

        // Profile scope claims
        if scopes.iter().any(|s| s == "profile") {
            filtered.name = self.name.clone();
            filtered.given_name = self.given_name.clone();
            filtered.family_name = self.family_name.clone();
            filtered.middle_name = self.middle_name.clone();
            filtered.nickname = self.nickname.clone();
            filtered.preferred_username = self.preferred_username.clone();
            filtered.profile = self.profile.clone();
            filtered.picture = self.picture.clone();
            filtered.website = self.website.clone();
            filtered.gender = self.gender.clone();
            filtered.birthdate = self.birthdate.clone();
            filtered.zoneinfo = self.zoneinfo.clone();
            filtered.locale = self.locale.clone();
            filtered.updated_at = self.updated_at;
        }

        // Email scope claims
        if scopes.iter().any(|s| s == "email") {
            filtered.email = self.email.clone();
            filtered.email_verified = self.email_verified;
        }

        // Phone scope claims
        if scopes.iter().any(|s| s == "phone") {
            filtered.phone_number = self.phone_number.clone();
            filtered.phone_number_verified = self.phone_number_verified;
        }

        // Address scope claims
        if scopes.iter().any(|s| s == "address") {
            filtered.address = self.address.clone();
        }

        filtered
    }
}

/// Address claim structure per OpenID Connect spec.
#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct AddressClaim {
    /// Full formatted address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub formatted: Option<String>,
    /// Street address.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub street_address: Option<String>,
    /// City/locality.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub locality: Option<String>,
    /// State/region.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// Postal/zip code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub postal_code: Option<String>,
    /// Country.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub country: Option<String>,
}

impl std::fmt::Debug for AddressClaim {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let populated_field_count = [
            self.formatted.is_some(),
            self.street_address.is_some(),
            self.locality.is_some(),
            self.region.is_some(),
            self.postal_code.is_some(),
            self.country.is_some(),
        ]
        .into_iter()
        .filter(|present| *present)
        .count();

        f.debug_struct("AddressClaim")
            .field("populated_field_count", &populated_field_count)
            .finish()
    }
}

// =============================================================================
// ID Token
// =============================================================================

/// ID Token claims (JWT payload).
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct IdTokenClaims {
    /// Issuer identifier.
    pub iss: String,
    /// Subject identifier.
    pub sub: String,
    /// Audience (client ID).
    pub aud: String,
    /// Expiration time (Unix timestamp).
    pub exp: i64,
    /// Issued at time (Unix timestamp).
    pub iat: i64,
    /// Authentication time (Unix timestamp).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_time: Option<i64>,
    /// Nonce from authorization request.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub nonce: Option<String>,
    /// Authentication Context Class Reference.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub acr: Option<String>,
    /// Authentication Methods References.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub amr: Option<Vec<String>>,
    /// Authorized party (client ID that was issued the token).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub azp: Option<String>,
    /// Access token hash (for hybrid flows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub at_hash: Option<String>,
    /// Code hash (for hybrid flows).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub c_hash: Option<String>,
    /// Additional user claims.
    #[serde(flatten)]
    pub user_claims: UserClaims,
}

impl std::fmt::Debug for IdTokenClaims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdTokenClaims")
            .field("issuer_len", &self.iss.len())
            .field("subject_len", &self.sub.len())
            .field("audience_len", &self.aud.len())
            .field("auth_time_present", &self.auth_time.is_some())
            .field("nonce_present", &self.nonce.is_some())
            .field("acr_present", &self.acr.is_some())
            .field("amr_count", &self.amr.as_ref().map_or(0, Vec::len))
            .field("authorized_party_present", &self.azp.is_some())
            .field("access_token_hash_present", &self.at_hash.is_some())
            .field("code_hash_present", &self.c_hash.is_some())
            .field("user_claims", &self.user_claims)
            .finish_non_exhaustive()
    }
}

/// A signed ID token.
#[derive(Clone)]
pub struct IdToken {
    /// The raw JWT string.
    pub raw: String,
    /// The parsed claims.
    pub claims: IdTokenClaims,
}

impl std::fmt::Debug for IdToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IdToken")
            .field("raw_len", &self.raw.len())
            .field("claims", &self.claims)
            .finish()
    }
}

// =============================================================================
// Discovery Document
// =============================================================================

/// OpenID Connect Discovery Document.
///
/// This is served at `/.well-known/openid-configuration`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoveryDocument {
    /// Issuer identifier URL.
    pub issuer: String,
    /// Authorization endpoint URL.
    pub authorization_endpoint: String,
    /// Token endpoint URL.
    pub token_endpoint: String,
    /// UserInfo endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub userinfo_endpoint: Option<String>,
    /// JWKs URI for public key retrieval.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub jwks_uri: Option<String>,
    /// Registration endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registration_endpoint: Option<String>,
    /// Revocation endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revocation_endpoint: Option<String>,
    /// Client authentication methods accepted by the revocation endpoint.
    ///
    /// The public-client `none` method is present only when the attached
    /// OAuth server admits public clients.
    pub revocation_endpoint_auth_methods_supported: Vec<String>,
    /// Whether every authorization response carries RFC 9207 `iss`.
    pub authorization_response_iss_parameter_supported: bool,
    /// Supported scopes.
    pub scopes_supported: Vec<String>,
    /// Supported response types.
    pub response_types_supported: Vec<String>,
    /// Supported response modes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_modes_supported: Option<Vec<String>>,
    /// Supported grant types.
    pub grant_types_supported: Vec<String>,
    /// Supported subject types.
    pub subject_types_supported: Vec<String>,
    /// Supported ID token signing algorithms.
    pub id_token_signing_alg_values_supported: Vec<String>,
    /// Supported token endpoint auth methods.
    pub token_endpoint_auth_methods_supported: Vec<String>,
    /// Supported claims.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub claims_supported: Option<Vec<String>>,
    /// Supported code challenge methods.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code_challenge_methods_supported: Option<Vec<String>>,
}

impl DiscoveryDocument {
    /// Creates a new discovery document with the given issuer and base URL.
    #[must_use]
    pub fn new(issuer: impl Into<String>, base_url: impl Into<String>) -> Self {
        let issuer = issuer.into();
        let base_url = base_url.into();
        // Endpoints are appended to the base, and `OAuthHttpRoutes` serves
        // `/authorize` for `https://a.example` and `https://a.example/` alike,
        // so a trailing slash must not be advertised as `//authorize`.
        let base = base_url.trim_end_matches('/');

        Self {
            issuer: issuer.clone(),
            authorization_endpoint: format!("{}/authorize", base),
            token_endpoint: format!("{}/token", base),
            userinfo_endpoint: Some(format!("{}/userinfo", base)),
            jwks_uri: None,
            registration_endpoint: None,
            revocation_endpoint: Some(format!("{}/revoke", base)),
            revocation_endpoint_auth_methods_supported: vec![
                "none".to_string(),
                "client_secret_post".to_string(),
                "client_secret_basic".to_string(),
            ],
            authorization_response_iss_parameter_supported: true,
            scopes_supported: vec![
                "openid".to_string(),
                "profile".to_string(),
                "email".to_string(),
            ],
            response_types_supported: vec!["code".to_string()],
            response_modes_supported: Some(vec!["query".to_string()]),
            grant_types_supported: vec![
                "authorization_code".to_string(),
                "refresh_token".to_string(),
            ],
            subject_types_supported: vec!["public".to_string()],
            id_token_signing_alg_values_supported: Vec::new(),
            token_endpoint_auth_methods_supported: vec![
                "none".to_string(),
                "client_secret_post".to_string(),
                "client_secret_basic".to_string(),
            ],
            claims_supported: Some(vec![
                "sub".to_string(),
                "iss".to_string(),
                "aud".to_string(),
                "exp".to_string(),
                "iat".to_string(),
                "name".to_string(),
                "email".to_string(),
                "email_verified".to_string(),
                "preferred_username".to_string(),
                "picture".to_string(),
            ]),
            code_challenge_methods_supported: Some(vec!["S256".to_string()]),
        }
    }
}

// =============================================================================
// Claims Provider
// =============================================================================

/// Trait for providing user claims.
pub trait ClaimsProvider: Send + Sync {
    /// Retrieves claims for a user by subject identifier.
    ///
    /// Returns `None` if the user is not found.
    fn get_claims(&self, subject: &str) -> Option<UserClaims>;
}

/// Simple in-memory claims provider.
#[derive(Default)]
pub struct InMemoryClaimsProvider {
    claims: RwLock<HashMap<String, UserClaims>>,
}

impl std::fmt::Debug for InMemoryClaimsProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let claim_count = self.claims.try_read().ok().map(|claims| claims.len());
        f.debug_struct("InMemoryClaimsProvider")
            .field("claim_count", &claim_count)
            .finish()
    }
}

impl InMemoryClaimsProvider {
    /// Creates a new empty claims provider.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds or updates claims for a user.
    pub fn set_claims(&self, claims: UserClaims) {
        if let Ok(mut guard) = self.claims.write() {
            guard.insert(claims.sub.clone(), claims);
        }
    }

    /// Removes claims for a user.
    pub fn remove_claims(&self, subject: &str) {
        if let Ok(mut guard) = self.claims.write() {
            guard.remove(subject);
        }
    }
}

impl ClaimsProvider for InMemoryClaimsProvider {
    fn get_claims(&self, subject: &str) -> Option<UserClaims> {
        self.claims
            .read()
            .ok()
            .and_then(|guard| guard.get(subject).cloned())
    }
}

/// Function-based claims provider.
pub struct FnClaimsProvider<F>
where
    F: Fn(&str) -> Option<UserClaims> + Send + Sync,
{
    func: F,
}

impl<F> FnClaimsProvider<F>
where
    F: Fn(&str) -> Option<UserClaims> + Send + Sync,
{
    /// Creates a new function-based claims provider.
    #[must_use]
    pub fn new(func: F) -> Self {
        Self { func }
    }
}

impl<F> ClaimsProvider for FnClaimsProvider<F>
where
    F: Fn(&str) -> Option<UserClaims> + Send + Sync,
{
    fn get_claims(&self, subject: &str) -> Option<UserClaims> {
        (self.func)(subject)
    }
}

impl ClaimsProvider for Arc<dyn ClaimsProvider> {
    fn get_claims(&self, subject: &str) -> Option<UserClaims> {
        (**self).get_claims(subject)
    }
}

// =============================================================================
// OIDC Errors
// =============================================================================

/// OIDC-specific errors.
pub enum OidcError {
    /// Underlying OAuth error.
    OAuth(OAuthError),
    /// Missing openid scope.
    MissingOpenIdScope,
    /// User claims not found.
    ClaimsNotFound(String),
    /// Claims provider returned an identity other than the requested subject.
    ClaimsSubjectMismatch,
    /// Token signing failed.
    SigningError(String),
    /// An external signer outcome, including its dispatch knowledge.
    #[cfg(feature = "builtin-auth-server")]
    ExternalSigning(JwsSigningError),
    /// Invalid ID token.
    InvalidIdToken(String),
}

impl std::fmt::Debug for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuth(error) => f.debug_tuple("OAuth").field(error).finish(),
            Self::MissingOpenIdScope => f.write_str("MissingOpenIdScope"),
            Self::ClaimsNotFound(subject) => f
                .debug_struct("ClaimsNotFound")
                .field("subject_len", &subject.len())
                .finish(),
            Self::ClaimsSubjectMismatch => f.write_str("ClaimsSubjectMismatch"),
            Self::SigningError(description) => f
                .debug_struct("SigningError")
                .field("description_len", &description.len())
                .finish(),
            #[cfg(feature = "builtin-auth-server")]
            Self::ExternalSigning(error) => f.debug_tuple("ExternalSigning").field(error).finish(),
            Self::InvalidIdToken(description) => f
                .debug_struct("InvalidIdToken")
                .field("description_len", &description.len())
                .finish(),
        }
    }
}

impl std::fmt::Display for OidcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OAuth(e) => write!(f, "OAuth error: {}", e),
            Self::MissingOpenIdScope => write!(f, "missing 'openid' scope"),
            Self::ClaimsNotFound(_) => f.write_str("claims not found for requested subject"),
            Self::ClaimsSubjectMismatch => {
                write!(f, "claims provider returned a mismatched subject")
            }
            Self::SigningError(_) => f.write_str("ID token signing failed"),
            #[cfg(feature = "builtin-auth-server")]
            Self::ExternalSigning(error) => write!(f, "external ID token signing failed: {error}"),
            Self::InvalidIdToken(_) => f.write_str("invalid ID token"),
        }
    }
}

impl std::error::Error for OidcError {}

impl From<OAuthError> for OidcError {
    fn from(err: OAuthError) -> Self {
        Self::OAuth(err)
    }
}

// =============================================================================
// OIDC Provider
// =============================================================================

/// OpenID Connect Provider.
///
/// This extends the OAuth server with OIDC identity features.
pub struct OidcProvider {
    /// Underlying OAuth server.
    oauth: Arc<OAuthServer>,
    /// OIDC configuration.
    config: OidcProviderConfig,
    /// Claims provider.
    claims_provider: RwLock<Option<Arc<dyn ClaimsProvider>>>,
    /// A non-forgeable external-signer activation. It remains absent until a
    /// caller supplies an exact publication/read-back of the selected public
    /// JWKS under the same immutable signer binding.
    #[cfg(feature = "builtin-auth-server")]
    signing_activation: RwLock<OidcIdTokenSigningState>,
    /// The embedding-owned verifier and durable CAS store.  There is no
    /// default: a missing dependency leaves activation fail-closed.
    #[cfg(feature = "builtin-auth-server")]
    signing_activation_dependencies: RwLock<Option<OidcSigningActivationDependencies>>,
}

/// Bounded, embedding-owned fetcher for every public JWKS endpoint bound to an
/// OIDC signer activation.
///
/// This is deliberately an external effect boundary.  Native route handlers
/// only serve bytes; they cannot mint a read-back record by parsing their own
/// in-memory publication buffer.
#[cfg(feature = "builtin-auth-server")]
pub trait OidcJwksReadBackVerifier: Send + Sync + 'static {
    /// Fetches every supplied exact URI and returns the observed URI, origin,
    /// response bytes, and monotonic publication generation.
    fn read_back<'a>(
        &'a self,
        cx: &'a Cx,
        endpoints: &'a [String],
    ) -> Pin<Box<dyn Future<Output = Result<Vec<JwksEndpointReadBack>, OidcError>> + Send + 'a>>;
}

/// Persistent compare-and-set record for an OIDC activation generation.
///
/// A store implementation must retain this record across process replacement.
/// The provider always reconstructs the cryptographic receipt through a fresh
/// external read-back; a stored generation alone never restores `Active`.
#[cfg(feature = "builtin-auth-server")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcSigningActivationStoreRecord {
    issuer: String,
    key_ring_generation: u64,
    activation_generation: u64,
    status: OidcSigningActivationStatus,
    maximum_id_token_expires_at: i64,
    key_id_maximum_id_token_expires_at: BTreeMap<String, OidcSigningKeyExpiry>,
}

/// One bounded, canonical public-key identity and its greatest issued
/// ID-token expiry. The identity contains public JWKS bytes only; it never
/// carries private key material or a signer backend.
#[cfg(feature = "builtin-auth-server")]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OidcSigningKeyExpiry {
    expires_at: i64,
    canonical_public_key_identity: Vec<u8>,
}

#[cfg(feature = "builtin-auth-server")]
impl OidcSigningKeyExpiry {
    /// Reconstructs a bounded public-key expiry watermark for an
    /// embedding-owned durable store.
    pub fn new(expires_at: i64, canonical_public_key_identity: Vec<u8>) -> Result<Self, OidcError> {
        if expires_at < 0
            || canonical_public_key_identity.is_empty()
            || canonical_public_key_identity.len() > MAX_JWKS_BYTES
        {
            return Err(OidcError::SigningError(
                "OIDC durable key expiry watermark is outside bounded admission".to_string(),
            ));
        }
        Ok(Self {
            expires_at,
            canonical_public_key_identity,
        })
    }

    /// Greatest ID-token expiry bound to this exact public key.
    #[must_use]
    pub const fn expires_at(&self) -> i64 {
        self.expires_at
    }

    /// Exact canonical public JWKS bytes for this one key.
    #[must_use]
    pub fn canonical_public_key_identity(&self) -> &[u8] {
        &self.canonical_public_key_identity
    }
}

/// Durable status for one profile-specific OIDC signing activation.
///
/// Any status or generation transition must advance the durable activation
/// generation. Issuers use this to fence stale process-local activation state
/// before and after external signing.
#[cfg(feature = "builtin-auth-server")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OidcSigningActivationStatus {
    /// The record authorizes issuance for its exact key-ring generation.
    Active,
    /// The record has closed issuance while retaining public verification
    /// material until its durable maximum ID-token expiry.
    Retiring,
    /// The record has been explicitly revoked; no local activation may issue.
    Revoked,
}

#[cfg(feature = "builtin-auth-server")]
impl OidcSigningActivationStoreRecord {
    /// Reconstructs one bounded durable record supplied by an embedding-owned
    /// store. This is the only public constructor; field mutation is not
    /// possible after validation.
    pub fn new(
        issuer: impl Into<String>,
        key_ring_generation: u64,
        activation_generation: u64,
        status: OidcSigningActivationStatus,
        maximum_id_token_expires_at: i64,
        key_id_maximum_id_token_expires_at: BTreeMap<String, OidcSigningKeyExpiry>,
    ) -> Result<Self, OidcError> {
        let issuer = issuer.into();
        if issuer.is_empty()
            || issuer.len() > crate::oauth::MAX_OAUTH_ISSUER_BYTES
            || issuer.bytes().any(|byte| byte.is_ascii_control())
            || key_ring_generation == 0
            || activation_generation == 0
            || maximum_id_token_expires_at < 0
            || key_id_maximum_id_token_expires_at.len() > MAX_JWKS_KEYS
            || key_id_maximum_id_token_expires_at
                .iter()
                .any(|(key_id, watermark)| {
                    key_id.is_empty()
                        || key_id.len() > MAX_KID_BYTES
                        || watermark.expires_at() > maximum_id_token_expires_at
                        || !canonical_oidc_key_identity_matches(
                            key_id,
                            watermark.canonical_public_key_identity(),
                        )
                })
            || key_id_maximum_id_token_expires_at
                .values()
                .try_fold(0usize, |total, watermark| {
                    total.checked_add(watermark.canonical_public_key_identity().len())
                })
                .is_none_or(|total| total > MAX_JWKS_BYTES)
        {
            return Err(OidcError::SigningError(
                "OIDC activation store record is outside bounded admission".to_string(),
            ));
        }
        Ok(Self {
            issuer,
            key_ring_generation,
            activation_generation,
            status,
            maximum_id_token_expires_at,
            key_id_maximum_id_token_expires_at,
        })
    }

    /// Exact issuer bound to the CAS record.
    #[must_use]
    pub fn issuer(&self) -> &str {
        &self.issuer
    }

    /// Key-ring generation admitted by the record.
    #[must_use]
    pub const fn key_ring_generation(&self) -> u64 {
        self.key_ring_generation
    }

    /// Monotonic CAS generation assigned by the store.
    #[must_use]
    pub const fn activation_generation(&self) -> u64 {
        self.activation_generation
    }

    /// Durable status used to fence issuance across provider instances.
    #[must_use]
    pub const fn status(&self) -> OidcSigningActivationStatus {
        self.status
    }

    /// Greatest ID-token expiry durably committed for this key-ring generation.
    #[must_use]
    pub const fn maximum_id_token_expires_at(&self) -> i64 {
        self.maximum_id_token_expires_at
    }

    /// Ordered per-key expiry watermarks durably carried across process
    /// replacement. Each entry is bounded and cannot exceed the aggregate
    /// maximum recorded above.
    #[must_use]
    pub fn key_id_maximum_id_token_expires_at(&self) -> &BTreeMap<String, OidcSigningKeyExpiry> {
        &self.key_id_maximum_id_token_expires_at
    }
}

/// Bounded durable CAS boundary for profile-specific OIDC signer activation.
///
/// Absence, a failed read, or a lost race is a fail-closed activation failure.
/// Implementations own durability, restore fencing, and process-instance
/// coordination; FastMCP deliberately supplies no implicit local persistence.
#[cfg(feature = "builtin-auth-server")]
pub trait OidcSigningActivationStore: Send + Sync + 'static {
    /// Reads the current record for the exact issuer.
    fn load(
        &self,
        cx: &Cx,
        issuer: &str,
    ) -> Result<Option<OidcSigningActivationStoreRecord>, OidcError>;

    /// Atomically writes `next` only if the currently stored activation
    /// generation equals `expected_generation` (`None` means no record).
    fn compare_and_set(
        &self,
        cx: &Cx,
        expected_generation: Option<u64>,
        next: OidcSigningActivationStoreRecord,
    ) -> Result<OidcSigningActivationStoreRecord, OidcError>;
}

#[cfg(feature = "builtin-auth-server")]
#[derive(Clone)]
struct OidcSigningActivationDependencies {
    verifier: Arc<dyn OidcJwksReadBackVerifier>,
    store: Arc<dyn OidcSigningActivationStore>,
}

/// Fail-closed state of the current process's ID-token signer activation.
///
/// Process restart deliberately returns to [`Self::Inactive`]; this server
/// does not deserialize an `Active` state from unverified storage. An embedding
/// must repeat publication and the public-endpoint read-back before signing can
/// resume.
#[cfg(feature = "builtin-auth-server")]
// The state lives behind one mutex-guarded slot and every transition moves
// the whole value; boxing a variant would only add indirection to the CAS.
#[allow(clippy::large_enum_variant)]
enum OidcIdTokenSigningState {
    /// No signer has been admitted for this issuer.
    Inactive,
    /// A signer and exact public endpoint are bound, but no bytes are public.
    Pending(OidcIdTokenSigningPending),
    /// Canonical public bytes are ready at the configured endpoint, but an
    /// endpoint read-back has not yet been observed and verified.
    Published(OidcIdTokenSigningPublished),
    /// The signer completed a canary that verified against endpoint read-back.
    Active(OidcIdTokenSigningActivation),
    /// An Active signer continues issuing while a successor ring progresses
    /// through Pending then Published read-back. The successor JWKS must
    /// retain the active key before it can replace this activation.
    Rotating {
        active: OidcIdTokenSigningActivation,
        successor: OidcIdTokenSigningSuccessor,
    },
    /// The published verification key remains visible, but issuance is closed
    /// after the issuer's recorded maximum token lifetime.
    Retiring(OidcIdTokenSigningActivation),
}

/// A signer/key/endpoint binding that cannot yet issue ID tokens.
#[cfg(feature = "builtin-auth-server")]
struct OidcIdTokenSigningPending {
    key_ring: Rs256PublicKeyRing,
    binding: Rs256SigningBinding,
    issuer: String,
    advertised_jwks_uris: Vec<String>,
    advertised_jwks_origins: Vec<String>,
    canonical_jwks: CanonicalRs256PublicJwksSet,
    dependencies: Option<OidcSigningActivationDependencies>,
}

/// A pending signer whose exact canonical public JWKS is available to the
/// configured public endpoint.
#[cfg(feature = "builtin-auth-server")]
struct OidcIdTokenSigningPublished {
    pending: OidcIdTokenSigningPending,
    published_jwks: Vec<u8>,
    publication_generation: u64,
}

/// A server-held active signer for one externally custodied ID-token key.
///
/// Its fields are private and it has no public constructor. It can only be
/// created after the selected signer's canonical public JWKS was served by the
/// exact public endpoint and a signer-produced canary verified through those
/// read-back public keys. This type neither implements a KMS/HSM adapter nor
/// contains private material.
#[cfg(feature = "builtin-auth-server")]
struct OidcIdTokenSigningActivation {
    published: OidcIdTokenSigningPublished,
    receipt: SigningActivationReceipt,
    activation_generation: u64,
    maximum_id_token_expires_at: i64,
    /// Per-key expiry watermarks let a successor retain every verification
    /// key that still protects an issued ID token while permitting keys whose
    /// artifacts have all expired to leave a later ring.
    live_key_expiries: BTreeMap<String, OidcSigningKeyExpiry>,
}

#[cfg(feature = "builtin-auth-server")]
enum OidcIdTokenSigningSuccessor {
    Pending(OidcIdTokenSigningPending),
    Published(OidcIdTokenSigningPublished),
}

#[cfg(feature = "builtin-auth-server")]
impl OidcIdTokenSigningState {
    fn published(&self) -> Option<&OidcIdTokenSigningPublished> {
        match self {
            Self::Published(published) => Some(published),
            Self::Active(active) => Some(&active.published),
            Self::Rotating {
                active,
                successor: OidcIdTokenSigningSuccessor::Pending(_),
            } => Some(&active.published),
            Self::Rotating {
                successor: OidcIdTokenSigningSuccessor::Published(published),
                ..
            } => Some(published),
            Self::Retiring(retiring) => Some(&retiring.published),
            Self::Inactive | Self::Pending(_) => None,
        }
    }

    fn active(&self) -> Option<&OidcIdTokenSigningActivation> {
        match self {
            Self::Active(active) => Some(active),
            Self::Rotating { active, .. } => Some(active),
            Self::Inactive | Self::Pending(_) | Self::Published(_) | Self::Retiring(_) => None,
        }
    }
}

#[cfg(feature = "builtin-auth-server")]
impl std::fmt::Debug for OidcIdTokenSigningState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inactive => formatter.write_str("OidcIdTokenSigningState::Inactive"),
            Self::Pending(_) => formatter.write_str("OidcIdTokenSigningState::Pending"),
            Self::Published(_) => formatter.write_str("OidcIdTokenSigningState::Published"),
            Self::Active(_) => formatter.write_str("OidcIdTokenSigningState::Active"),
            Self::Rotating { .. } => formatter.write_str("OidcIdTokenSigningState::Rotating"),
            Self::Retiring(_) => formatter.write_str("OidcIdTokenSigningState::Retiring"),
        }
    }
}

#[cfg(feature = "builtin-auth-server")]
impl std::fmt::Debug for OidcIdTokenSigningActivation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let published = &self.published;
        formatter
            .debug_struct("OidcIdTokenSigningActivation")
            .field("binding", &published.pending.binding)
            .field("issuer_bytes", &published.pending.issuer.len())
            .field(
                "advertised_jwks_uri_bytes",
                &published
                    .pending
                    .advertised_jwks_uris
                    .iter()
                    .map(String::len)
                    .sum::<usize>(),
            )
            .field(
                "advertised_jwks_origin_bytes",
                &published
                    .pending
                    .advertised_jwks_origins
                    .iter()
                    .map(String::len)
                    .sum::<usize>(),
            )
            .field("published_jwks_bytes", &published.published_jwks.len())
            .field(
                "key_ring_generation",
                &published.pending.key_ring.generation(),
            )
            .field("publication_generation", &published.publication_generation)
            .field("activation_generation", &self.activation_generation)
            .finish()
    }
}

fn validate_oidc_config(config: &OidcProviderConfig) -> Result<(), OidcError> {
    // Reuse the OAuth issuer admission policy so OIDC discovery cannot publish
    // a non-HTTPS, non-canonical, or otherwise unsafe issuer spelling.
    validate_oauth_issuer(&config.issuer).map_err(OidcError::from)
}

impl OidcProvider {
    /// Creates a new OIDC provider with the given OAuth server.
    ///
    /// # Errors
    ///
    /// Returns an error when the OIDC issuer is unsafe or does not exactly
    /// match the underlying OAuth server issuer.
    pub fn new(oauth: Arc<OAuthServer>, config: OidcProviderConfig) -> Result<Self, OidcError> {
        validate_oidc_config(&config)?;
        if config.issuer != oauth.config().issuer {
            return Err(OidcError::OAuth(OAuthError::ServerError(
                "OIDC issuer must exactly match the OAuth server issuer".to_string(),
            )));
        }
        Ok(Self {
            oauth,
            config,
            claims_provider: RwLock::new(None),
            #[cfg(feature = "builtin-auth-server")]
            signing_activation: RwLock::new(OidcIdTokenSigningState::Inactive),
            #[cfg(feature = "builtin-auth-server")]
            signing_activation_dependencies: RwLock::new(None),
        })
    }

    /// Creates a new OIDC provider with default configuration.
    ///
    /// # Errors
    ///
    /// Returns an error when the underlying OAuth server has an unsafe issuer.
    pub fn with_defaults(oauth: Arc<OAuthServer>) -> Result<Self, OidcError> {
        let config = OidcProviderConfig {
            issuer: oauth.config().issuer.clone(),
            ..OidcProviderConfig::default()
        };
        Self::new(oauth, config)
    }

    /// Returns the OIDC configuration.
    #[must_use]
    pub fn config(&self) -> &OidcProviderConfig {
        &self.config
    }

    /// Returns a reference to the underlying OAuth server.
    #[must_use]
    pub fn oauth(&self) -> &Arc<OAuthServer> {
        &self.oauth
    }

    /// Sets the claims provider.
    pub fn set_claims_provider<P: ClaimsProvider + 'static>(&self, provider: P) {
        if let Ok(mut guard) = self.claims_provider.write() {
            *guard = Some(Arc::new(provider));
        }
    }

    /// Sets a function-based claims provider.
    pub fn set_claims_fn<F>(&self, func: F)
    where
        F: Fn(&str) -> Option<UserClaims> + Send + Sync + 'static,
    {
        self.set_claims_provider(FnClaimsProvider::new(func));
    }

    /// Configures the explicit external read-back and durable-CAS boundaries
    /// required before this provider can activate an ID-token signer.
    #[cfg(feature = "builtin-auth-server")]
    pub fn set_id_token_signing_activation_dependencies(
        &self,
        verifier: Arc<dyn OidcJwksReadBackVerifier>,
        store: Arc<dyn OidcSigningActivationStore>,
    ) -> Result<(), OidcError> {
        let mut dependencies = self.signing_activation_dependencies.write().map_err(|_| {
            OidcError::SigningError(
                "OIDC signing activation dependencies are unavailable".to_string(),
            )
        })?;
        if dependencies.is_some() {
            return Err(OidcError::SigningError(
                "OIDC signing activation dependencies are already configured".to_string(),
            ));
        }
        *dependencies = Some(OidcSigningActivationDependencies { verifier, store });
        Ok(())
    }

    /// Starts a sealed Pending activation for one externally custodied signer.
    ///
    /// This is intentionally not enough to issue tokens. The caller must next
    /// publish the exact canonical bytes through the configured public route,
    /// let that route observe a read-back, and then activate its canary.
    #[cfg(feature = "builtin-auth-server")]
    pub fn begin_id_token_signing_activation(
        &self,
        signer: Arc<ExternalRs256Signer>,
        advertised_jwks_uri: &str,
    ) -> Result<(), OidcError> {
        let signer_ring_generation = signer.binding().ring_generation();
        let key_ring = Rs256PublicKeyRing::new(signer, Vec::new(), signer_ring_generation)
            .map_err(|_| {
                OidcError::SigningError("unable to admit OIDC signer key ring".to_string())
            })?;
        self.begin_id_token_signing_key_ring_activation(
            key_ring,
            vec![advertised_jwks_uri.to_string()],
        )
    }

    /// Starts Pending activation for a selected active signer plus retained
    /// verification overlap.  Every endpoint is independently bound and must
    /// later appear in the external verifier's receipt.
    #[cfg(feature = "builtin-auth-server")]
    pub fn begin_id_token_signing_key_ring_activation(
        &self,
        key_ring: Rs256PublicKeyRing,
        advertised_jwks_uris: Vec<String>,
    ) -> Result<(), OidcError> {
        if advertised_jwks_uris.is_empty() {
            return Err(OidcError::SigningError(
                "OIDC signer activation requires at least one advertised JWKS endpoint".to_string(),
            ));
        }
        let mut admitted_uris = Vec::with_capacity(advertised_jwks_uris.len());
        let mut admitted_origins = Vec::with_capacity(advertised_jwks_uris.len());
        for uri in advertised_jwks_uris {
            let (uri, origin) = validate_advertised_jwks_uri(&self.config.issuer, &uri)?;
            if admitted_uris.iter().any(|existing| existing == &uri) {
                return Err(OidcError::SigningError(
                    "OIDC signer activation has duplicate advertised JWKS endpoints".to_string(),
                ));
            }
            admitted_uris.push(uri);
            admitted_origins.push(origin);
        }
        let canonical_jwks = key_ring.canonical_public_jwks().map_err(|_| {
            OidcError::SigningError("unable to canonicalize signing JWKS".to_string())
        })?;
        let dependencies = self
            .signing_activation_dependencies
            .read()
            .map_err(|_| {
                OidcError::SigningError(
                    "OIDC signing activation dependencies are unavailable".to_string(),
                )
            })?
            .clone();
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        if !matches!(*slot, OidcIdTokenSigningState::Inactive) {
            return Err(OidcError::SigningError(
                "OIDC signing activation is already in progress or active".to_string(),
            ));
        }
        *slot = OidcIdTokenSigningState::Pending(OidcIdTokenSigningPending {
            binding: key_ring.active_signer().binding(),
            key_ring,
            issuer: self.config.issuer.clone(),
            advertised_jwks_uris: admitted_uris,
            advertised_jwks_origins: admitted_origins,
            canonical_jwks,
            dependencies,
        });
        Ok(())
    }

    /// Begins a live successor transition without withdrawing the currently
    /// Active signer. The successor ring must retain the exact active signer
    /// as a public verification key and advance the key-ring generation.
    #[cfg(feature = "builtin-auth-server")]
    pub fn begin_id_token_signing_key_ring_rotation(
        &self,
        key_ring: Rs256PublicKeyRing,
        advertised_jwks_uris: Vec<String>,
    ) -> Result<(), OidcError> {
        if advertised_jwks_uris.is_empty() {
            return Err(OidcError::SigningError(
                "OIDC signer rotation requires at least one advertised JWKS endpoint".to_string(),
            ));
        }
        let mut admitted_uris = Vec::with_capacity(advertised_jwks_uris.len());
        let mut admitted_origins = Vec::with_capacity(advertised_jwks_uris.len());
        for uri in advertised_jwks_uris {
            let (uri, origin) = validate_advertised_jwks_uri(&self.config.issuer, &uri)?;
            if admitted_uris.iter().any(|existing| existing == &uri) {
                return Err(OidcError::SigningError(
                    "OIDC signer rotation has duplicate advertised JWKS endpoints".to_string(),
                ));
            }
            admitted_uris.push(uri);
            admitted_origins.push(origin);
        }
        let canonical_jwks = key_ring.canonical_public_jwks().map_err(|_| {
            OidcError::SigningError("unable to canonicalize successor signing JWKS".to_string())
        })?;
        let successor_identities = oidc_key_ring_public_identities(&key_ring)?;
        let now = oidc_unix_timestamp()?;
        let dependencies = self
            .signing_activation_dependencies
            .read()
            .map_err(|_| {
                OidcError::SigningError(
                    "OIDC signing activation dependencies are unavailable".to_string(),
                )
            })?
            .clone();
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let state = std::mem::replace(&mut *slot, OidcIdTokenSigningState::Inactive);
        let OidcIdTokenSigningState::Active(active) = state else {
            *slot = state;
            return Err(OidcError::SigningError(
                "OIDC signer rotation requires an Active generation".to_string(),
            ));
        };
        let current = active.published.pending.key_ring.active_signer();
        let drops_live_key = active.live_key_expiries.iter().any(|(key_id, watermark)| {
            (key_id == current.key_id() || watermark.expires_at() > now)
                && (!key_ring.retains_key_from(&active.published.pending.key_ring, key_id)
                    || successor_identities.get(key_id).is_none_or(|identity| {
                        identity.as_slice() != watermark.canonical_public_key_identity()
                    }))
        });
        if key_ring.generation() <= active.published.pending.key_ring.generation()
            || key_ring.active_signer().binding() == current.binding()
            || drops_live_key
        {
            *slot = OidcIdTokenSigningState::Active(active);
            return Err(OidcError::SigningError(
                "OIDC successor ring must advance and retain every live public key".to_string(),
            ));
        }
        let pending = OidcIdTokenSigningPending {
            binding: key_ring.active_signer().binding(),
            key_ring,
            issuer: self.config.issuer.clone(),
            advertised_jwks_uris: admitted_uris,
            advertised_jwks_origins: admitted_origins,
            canonical_jwks,
            dependencies,
        };
        *slot = OidcIdTokenSigningState::Rotating {
            active,
            successor: OidcIdTokenSigningSuccessor::Pending(pending),
        };
        Ok(())
    }

    /// Publishes the exact signer-bound canonical JWKS to this provider's
    /// public endpoint state. Copied or stale bytes leave the state Pending.
    #[cfg(feature = "builtin-auth-server")]
    pub fn publish_id_token_signing_jwks(
        &self,
        published_jwks: CanonicalRs256PublicJwks,
    ) -> Result<(), OidcError> {
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let state = std::mem::replace(&mut *slot, OidcIdTokenSigningState::Inactive);
        let OidcIdTokenSigningState::Pending(pending) = state else {
            *slot = state;
            return Err(OidcError::SigningError(
                "OIDC signing JWKS publication requires a Pending activation".to_string(),
            ));
        };
        if published_jwks.binding() != pending.binding
            || published_jwks.as_bytes() != pending.canonical_jwks.as_bytes()
        {
            *slot = OidcIdTokenSigningState::Pending(pending);
            return Err(OidcError::SigningError(
                "OIDC signing JWKS publication does not match the pending signer binding"
                    .to_string(),
            ));
        }
        *slot = OidcIdTokenSigningState::Published(OidcIdTokenSigningPublished {
            published_jwks: published_jwks.as_bytes().to_vec(),
            pending,
            publication_generation: published_jwks.binding().ring_generation(),
        });
        Ok(())
    }

    /// Publishes the exact canonical active-plus-retained JWKS for a key-ring
    /// activation.  This is the rotation-capable publication entry point.
    #[cfg(feature = "builtin-auth-server")]
    pub fn publish_id_token_signing_key_ring_jwks(
        &self,
        published_jwks: CanonicalRs256PublicJwksSet,
    ) -> Result<(), OidcError> {
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let state = std::mem::replace(&mut *slot, OidcIdTokenSigningState::Inactive);
        let (pending, active) = match state {
            OidcIdTokenSigningState::Pending(pending) => (pending, None),
            OidcIdTokenSigningState::Rotating {
                active,
                successor: OidcIdTokenSigningSuccessor::Pending(pending),
            } => (pending, Some(active)),
            state => {
                *slot = state;
                return Err(OidcError::SigningError(
                    "OIDC signing JWKS publication requires a Pending activation".to_string(),
                ));
            }
        };
        if published_jwks.generation() != pending.key_ring.generation()
            || published_jwks.as_bytes() != pending.canonical_jwks.as_bytes()
        {
            *slot = match active {
                Some(active) => OidcIdTokenSigningState::Rotating {
                    active,
                    successor: OidcIdTokenSigningSuccessor::Pending(pending),
                },
                None => OidcIdTokenSigningState::Pending(pending),
            };
            return Err(OidcError::SigningError(
                "OIDC signing JWKS publication does not match the pending key ring".to_string(),
            ));
        }
        let published = OidcIdTokenSigningPublished {
            published_jwks: published_jwks.as_bytes().to_vec(),
            publication_generation: published_jwks.generation(),
            pending,
        };
        *slot = match active {
            Some(active) => OidcIdTokenSigningState::Rotating {
                active,
                successor: OidcIdTokenSigningSuccessor::Published(published),
            },
            None => OidcIdTokenSigningState::Published(published),
        };
        Ok(())
    }

    /// Returns the exact public JWKS URI bound while this activation is
    /// Pending, Published, or Active. Native HTTP configuration uses this to
    /// install one non-inferred route; callers cannot substitute a Host header
    /// or copied endpoint spelling.
    #[cfg(feature = "builtin-auth-server")]
    pub(crate) fn advertised_id_token_jwks_uri(&self) -> Result<String, OidcError> {
        let slot = self.signing_activation.read().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        match &*slot {
            OidcIdTokenSigningState::Pending(pending) => {
                Ok(pending.advertised_jwks_uris[0].clone())
            }
            OidcIdTokenSigningState::Published(published) => {
                Ok(published.pending.advertised_jwks_uris[0].clone())
            }
            OidcIdTokenSigningState::Active(active) => {
                Ok(active.published.pending.advertised_jwks_uris[0].clone())
            }
            OidcIdTokenSigningState::Rotating {
                active,
                successor: OidcIdTokenSigningSuccessor::Pending(_),
            } => Ok(active.published.pending.advertised_jwks_uris[0].clone()),
            OidcIdTokenSigningState::Rotating {
                successor: OidcIdTokenSigningSuccessor::Published(published),
                ..
            } => Ok(published.pending.advertised_jwks_uris[0].clone()),
            OidcIdTokenSigningState::Retiring(retiring) => {
                Ok(retiring.published.pending.advertised_jwks_uris[0].clone())
            }
            OidcIdTokenSigningState::Inactive => Err(OidcError::SigningError(
                "OIDC signing activation has no advertised JWKS endpoint".to_string(),
            )),
        }
    }

    /// Returns exact canonical bytes for the bound public JWKS route.
    ///
    /// This is intentionally side-effect free.  Serving a response is not an
    /// endpoint read-back and can never mint activation evidence; only the
    /// embedding-owned [`OidcJwksReadBackVerifier`] may do that.
    #[cfg(feature = "builtin-auth-server")]
    pub(crate) fn published_jwks_document(
        &self,
        advertised_jwks_uri: &str,
    ) -> Result<Vec<u8>, OidcError> {
        let slot = self.signing_activation.read().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let Some(published) = slot.published() else {
            return Err(OidcError::SigningError(
                "OIDC signing JWKS is not published".to_string(),
            ));
        };
        if !published
            .pending
            .advertised_jwks_uris
            .iter()
            .any(|uri| uri == advertised_jwks_uri)
        {
            return Err(OidcError::SigningError(
                "OIDC public JWKS request did not use the activated endpoint".to_string(),
            ));
        }
        Ok(published.published_jwks.clone())
    }

    /// Completes Active admission through an external endpoint read-back and
    /// durable compare-and-set.  A route handler's local buffer is never
    /// sufficient evidence.
    #[cfg(feature = "builtin-auth-server")]
    pub async fn activate_id_token_signing(
        &self,
        cx: &Cx,
        deadline: ExternalRs256SigningDeadline,
    ) -> Result<(), OidcError> {
        let (
            key_ring,
            binding,
            expected,
            endpoints,
            origins,
            issuer,
            dependencies,
            publication_generation,
            rotating,
        ) = {
            let slot = self.signing_activation.read().map_err(|_| {
                OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
            })?;
            let (published, rotating) = match &*slot {
                OidcIdTokenSigningState::Published(published) => (published, false),
                OidcIdTokenSigningState::Rotating {
                    successor: OidcIdTokenSigningSuccessor::Published(published),
                    ..
                } => (published, true),
                _ => {
                    return Err(OidcError::SigningError(
                        "OIDC signing activation requires published JWKS read-back".to_string(),
                    ));
                }
            };
            (
                published.pending.key_ring.clone(),
                published.pending.binding,
                published.pending.canonical_jwks.clone(),
                published.pending.advertised_jwks_uris.clone(),
                published.pending.advertised_jwks_origins.clone(),
                published.pending.issuer.clone(),
                published.pending.dependencies.clone(),
                published.publication_generation,
                rotating,
            )
        };
        let dependencies = dependencies.ok_or_else(|| {
            OidcError::SigningError(
                "OIDC signer activation requires an external read-back verifier and durable store"
                    .to_string(),
            )
        })?;
        let canary = oidc_signing_canary_claims()?;
        let signed = key_ring
            .active_signer()
            .sign(cx, JwsSigningProfile::OidcIdToken, canary, deadline)
            .await
            .map_err(OidcError::ExternalSigning)?;
        if signed.binding() != binding {
            return Err(OidcError::SigningError(
                "OIDC signing canary returned a stale signer binding".to_string(),
            ));
        }
        cx.checkpoint().map_err(|_| {
            OidcError::SigningError(
                "OIDC signing activation cancelled after canary dispatch".to_string(),
            )
        })?;
        let canary = signed.into_compact_jws();
        let observed = dependencies.verifier.read_back(cx, &endpoints).await?;
        cx.checkpoint().map_err(|_| {
            OidcError::SigningError("OIDC activation cancelled after JWKS read-back".to_string())
        })?;
        let receipt = SigningActivationReceipt::verify(
            SigningActivationProfile::OidcIdToken,
            issuer.clone(),
            &expected,
            &endpoints,
            &origins,
            observed,
            canary,
        )
        .map_err(|_| {
            OidcError::SigningError(
                "OIDC signing canary failed public JWKS verification".to_string(),
            )
        })?;

        cx.checkpoint().map_err(|_| {
            OidcError::SigningError("OIDC activation cancelled before durable CAS".to_string())
        })?;
        let prior = dependencies.store.load(cx, &issuer)?;
        if prior.as_ref().is_some_and(|record| {
            record.issuer() != issuer
                || record.status() != OidcSigningActivationStatus::Active
                || record.key_ring_generation() > expected.generation()
        }) {
            return Err(OidcError::SigningError(
                "OIDC activation store rejected a stale or rollback key-ring generation"
                    .to_string(),
            ));
        }
        let now = oidc_unix_timestamp()?;
        let successor_key_identities = oidc_key_ring_public_identities(&key_ring)?;
        let mut key_id_maximum_id_token_expires_at =
            prior.as_ref().map_or_else(BTreeMap::new, |record| {
                record.key_id_maximum_id_token_expires_at().clone()
            });
        if key_id_maximum_id_token_expires_at
            .iter()
            .any(|(key_id, watermark)| {
                watermark.expires_at() > now
                    && successor_key_identities.get(key_id).is_none_or(|identity| {
                        identity.as_slice() != watermark.canonical_public_key_identity()
                    })
            })
        {
            return Err(OidcError::SigningError(
                "OIDC activation key ring omits or substitutes a durably live verification key"
                    .to_string(),
            ));
        }
        key_id_maximum_id_token_expires_at.retain(|key_id, watermark| {
            watermark.expires_at() > now || successor_key_identities.contains_key(key_id)
        });
        for (key_id, identity) in successor_key_identities {
            let replaces_expired_identity = key_id_maximum_id_token_expires_at
                .get(&key_id)
                .is_some_and(|watermark| {
                    watermark.expires_at() <= now
                        && watermark.canonical_public_key_identity() != identity.as_slice()
                });
            if replaces_expired_identity
                || !key_id_maximum_id_token_expires_at.contains_key(&key_id)
            {
                key_id_maximum_id_token_expires_at
                    .insert(key_id, OidcSigningKeyExpiry::new(0, identity)?);
            }
        }
        let expected_activation_generation = prior
            .as_ref()
            .map(OidcSigningActivationStoreRecord::activation_generation);
        let next_generation = expected_activation_generation
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| {
                OidcError::SigningError("OIDC activation generation is exhausted".to_string())
            })?;
        let next = OidcSigningActivationStoreRecord::new(
            issuer.clone(),
            expected.generation(),
            next_generation,
            OidcSigningActivationStatus::Active,
            prior.as_ref().map_or(
                0,
                OidcSigningActivationStoreRecord::maximum_id_token_expires_at,
            ),
            key_id_maximum_id_token_expires_at,
        )?;
        let committed =
            dependencies
                .store
                .compare_and_set(cx, expected_activation_generation, next.clone())?;
        if committed != next {
            return Err(OidcError::SigningError(
                "OIDC activation store returned a mismatched CAS record".to_string(),
            ));
        }

        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let state = std::mem::replace(&mut *slot, OidcIdTokenSigningState::Inactive);
        let (published, previous_active) = match state {
            OidcIdTokenSigningState::Published(published) if !rotating => (published, None),
            OidcIdTokenSigningState::Rotating {
                active,
                successor: OidcIdTokenSigningSuccessor::Published(published),
            } if rotating => (published, Some(active)),
            state => {
                *slot = state;
                return Err(OidcError::SigningError(
                    "OIDC signing activation changed while its canary was in flight".to_string(),
                ));
            }
        };
        if published.pending.binding != binding
            || published.publication_generation != publication_generation
            || published.pending.key_ring.generation() != expected.generation()
        {
            *slot = match previous_active {
                Some(active) => OidcIdTokenSigningState::Rotating {
                    active,
                    successor: OidcIdTokenSigningSuccessor::Published(published),
                },
                None => OidcIdTokenSigningState::Published(published),
            };
            return Err(OidcError::SigningError(
                "OIDC signing activation publication changed while its canary was in flight"
                    .to_string(),
            ));
        }
        *slot = OidcIdTokenSigningState::Active(OidcIdTokenSigningActivation {
            published,
            receipt,
            activation_generation: committed.activation_generation,
            maximum_id_token_expires_at: committed.maximum_id_token_expires_at,
            live_key_expiries: committed.key_id_maximum_id_token_expires_at().clone(),
        });
        Ok(())
    }

    /// Closes issuance and enters `Retiring` only after the recorded maximum
    /// lifetime of every ID token signed by this generation.  Verification
    /// material remains available to the public JWKS route while retirement is
    /// observed; callers must install a successor generation separately.
    #[cfg(feature = "builtin-auth-server")]
    pub fn retire_id_token_signing_generation(
        &self,
        cx: &Cx,
        now_unix_seconds: i64,
    ) -> Result<(), OidcError> {
        let (dependencies, issuer, key_ring_generation, activation_generation) = {
            let slot = self.signing_activation.read().map_err(|_| {
                OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
            })?;
            let OidcIdTokenSigningState::Active(active) = &*slot else {
                return Err(OidcError::SigningError(
                    "OIDC signing retirement requires an unrotated Active generation".to_string(),
                ));
            };
            (
                active
                    .published
                    .pending
                    .dependencies
                    .clone()
                    .ok_or_else(|| {
                        OidcError::SigningError(
                            "OIDC active signer lost its durable activation dependencies"
                                .to_string(),
                        )
                    })?,
                active.published.pending.issuer.clone(),
                active.published.pending.key_ring.generation(),
                active.activation_generation,
            )
        };
        let durable = fence_active_oidc_activation_store(
            cx,
            &dependencies,
            &issuer,
            key_ring_generation,
            activation_generation,
        )?;
        if durable.maximum_id_token_expires_at() > now_unix_seconds {
            return Err(OidcError::SigningError(
                "OIDC signing key cannot retire before the durable maximum ID-token expiry"
                    .to_string(),
            ));
        }
        let next_generation = durable
            .activation_generation()
            .checked_add(1)
            .ok_or_else(|| {
                OidcError::SigningError(
                    "OIDC durable activation generation is exhausted".to_string(),
                )
            })?;
        let next = OidcSigningActivationStoreRecord::new(
            issuer,
            key_ring_generation,
            next_generation,
            OidcSigningActivationStatus::Retiring,
            durable.maximum_id_token_expires_at(),
            durable.key_id_maximum_id_token_expires_at().clone(),
        )?;
        let committed = dependencies.store.compare_and_set(
            cx,
            Some(durable.activation_generation()),
            next.clone(),
        )?;
        if committed != next {
            return Err(OidcError::SigningError(
                "OIDC durable retirement CAS fence was lost".to_string(),
            ));
        }
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let state = std::mem::replace(&mut *slot, OidcIdTokenSigningState::Inactive);
        let OidcIdTokenSigningState::Active(mut active) = state else {
            *slot = state;
            return Err(OidcError::SigningError(
                "OIDC signing retirement requires an Active generation".to_string(),
            ));
        };
        if active.activation_generation != durable.activation_generation()
            || active.published.pending.key_ring.generation() != key_ring_generation
        {
            *slot = OidcIdTokenSigningState::Active(active);
            return Err(OidcError::SigningError(
                "OIDC signing activation changed before retirement".to_string(),
            ));
        }
        active.activation_generation = committed.activation_generation();
        active.maximum_id_token_expires_at = committed.maximum_id_token_expires_at();
        *slot = OidcIdTokenSigningState::Retiring(active);
        Ok(())
    }

    /// Returns exact public JWKS bytes only after their matching activation is
    /// Active. A pending/publication read-back never exposes a usable signer.
    #[cfg(feature = "builtin-auth-server")]
    pub fn activated_jwks_document(&self) -> Result<Vec<u8>, OidcError> {
        let slot = self.signing_activation.read().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let active = slot.active().ok_or_else(|| {
            OidcError::SigningError("OIDC signing activation is not Active".to_string())
        })?;
        Ok(active.published.published_jwks.clone())
    }

    /// Generates the discovery document.
    #[must_use]
    pub fn discovery_document(&self, base_url: impl Into<String>) -> DiscoveryDocument {
        let base_url = base_url.into();
        let mut doc = DiscoveryDocument::new(&self.config.issuer, base_url.as_str());
        doc.scopes_supported = self.config.supported_scopes.clone();
        if self.oauth.config().allow_public_clients {
            let base = base_url.trim_end_matches('/');
            doc.registration_endpoint = Some(format!("{base}/register"));
        }
        doc.claims_supported = Some(self.config.supported_claims.clone());
        // Discovery must describe this exact OAuthServer configuration, not
        // merely the broad capabilities of DiscoveryDocument::new. Public
        // clients authenticate token and revocation requests with client_id
        // and no secret; confidential clients retain both secret methods.
        if !self.oauth.config().allow_public_clients {
            doc.token_endpoint_auth_methods_supported
                .retain(|method| method != "none");
            doc.revocation_endpoint_auth_methods_supported
                .retain(|method| method != "none");
        }
        #[cfg(feature = "builtin-auth-server")]
        if let Ok(slot) = self.signing_activation.read() {
            if let (Some(_), Some(published)) = (slot.active(), slot.published()) {
                doc.id_token_signing_alg_values_supported = vec!["RS256".to_string()];
                doc.jwks_uri = Some(published.pending.advertised_jwks_uris[0].clone());
            }
        }
        doc
    }

    // -------------------------------------------------------------------------
    // ID Token Issuance
    // -------------------------------------------------------------------------

    /// Issues one OIDC ID-token candidate through the active external RS256
    /// signer. The returned compact JWS is self-verified by the signer facade
    /// and then verified again against the exact JWKS publication read-back.
    ///
    /// This is intentionally asynchronous and accepts the caller's [`Cx`];
    /// it never creates a runtime or substitutes an in-process signing key.
    #[cfg(feature = "builtin-auth-server")]
    pub async fn issue_id_token(
        &self,
        cx: &Cx,
        access_token: &str,
        nonce: Option<&str>,
        deadline: ExternalRs256SigningDeadline,
    ) -> Result<IdToken, OidcError> {
        let access_token_credential = access_token;
        let access_token = self.validated_oidc_access_token(access_token)?;
        let subject = access_token
            .subject
            .as_deref()
            .ok_or_else(|| OidcError::ClaimsNotFound("no subject in access token".to_string()))?;
        validate_oidc_nonce(nonce)?;
        let user_claims = self.get_user_claims(subject, &access_token.scopes)?;
        let now = oidc_unix_timestamp()?;
        let expires_in = access_token
            .expires_at
            .saturating_duration_since(Instant::now())
            .as_secs();
        let expires_in = i64::try_from(expires_in).map_err(|_| {
            OidcError::InvalidIdToken("access-token lifetime exceeds ID-token range".to_string())
        })?;
        let exp = now.checked_add(expires_in).ok_or_else(|| {
            OidcError::InvalidIdToken("ID-token expiry exceeds timestamp range".to_string())
        })?;
        if exp <= now {
            return Err(OidcError::InvalidIdToken(
                "access token is expired before ID-token issuance".to_string(),
            ));
        }
        let claims = IdTokenClaims {
            iss: self.config.issuer.clone(),
            sub: subject.to_string(),
            aud: access_token.client_id.clone(),
            exp,
            iat: now,
            auth_time: None,
            nonce: nonce.map(str::to_string),
            acr: None,
            amr: None,
            azp: None,
            at_hash: None,
            c_hash: None,
            user_claims,
        };
        let signing_claims = id_token_signing_claims(&claims)?;
        let (
            signer,
            binding,
            read_back_keys,
            dependencies,
            activation_generation,
            key_ring_generation,
        ) = {
            let slot = self.signing_activation.read().map_err(|_| {
                OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
            })?;
            let activation = slot.active().ok_or_else(|| {
                OidcError::SigningError("OIDC signing activation is required".to_string())
            })?;
            if activation.published.pending.issuer != self.config.issuer
                || activation.published.pending.binding
                    != activation
                        .published
                        .pending
                        .key_ring
                        .active_signer()
                        .binding()
                || !activation.receipt.applies_to(
                    SigningActivationProfile::OidcIdToken,
                    &self.config.issuer,
                    &activation.published.pending.canonical_jwks,
                    &activation.published.pending.advertised_jwks_uris,
                    &activation.published.pending.advertised_jwks_origins,
                )
            {
                return Err(OidcError::SigningError(
                    "OIDC signing activation no longer matches the selected signer".to_string(),
                ));
            }
            (
                Arc::clone(activation.published.pending.key_ring.active_signer()),
                activation.published.pending.binding,
                AdmittedRsaJwks::from_json(&activation.published.published_jwks).map_err(|_| {
                    OidcError::SigningError(
                        "OIDC signing activation lost its public JWKS read-back".to_string(),
                    )
                })?,
                activation
                    .published
                    .pending
                    .dependencies
                    .clone()
                    .ok_or_else(|| {
                        OidcError::SigningError(
                            "OIDC active signer lost its durable activation dependencies"
                                .to_string(),
                        )
                    })?,
                activation.activation_generation,
                activation.published.pending.key_ring.generation(),
            )
        };
        let durable_before_sign = fence_active_oidc_activation_store(
            cx,
            &dependencies,
            &self.config.issuer,
            key_ring_generation,
            activation_generation,
        )?;
        let signer_key_id = signer.key_id().to_string();
        let signer_key_identity = signer
            .canonical_public_jwks()
            .map_err(|_| {
                OidcError::SigningError(
                    "OIDC active signer has no canonical public key identity".to_string(),
                )
            })?
            .as_bytes()
            .to_vec();
        let signed = signer
            .sign(cx, JwsSigningProfile::OidcIdToken, signing_claims, deadline)
            .await
            .map_err(OidcError::ExternalSigning)?;
        if signed.binding() != binding {
            return Err(OidcError::SigningError(
                "external ID-token signing binding changed before exposure".to_string(),
            ));
        }
        cx.checkpoint().map_err(|_| {
            OidcError::SigningError("OIDC request cancelled after external signing".to_string())
        })?;
        let revalidated = self.validated_oidc_access_token(access_token_credential)?;
        if !same_id_token_authorization(&access_token, &revalidated) {
            return Err(OidcError::SigningError(
                "OIDC access-token authorization changed during external signing".to_string(),
            ));
        }
        let next_activation_generation = durable_before_sign
            .activation_generation()
            .checked_add(1)
            .ok_or_else(|| {
                OidcError::SigningError(
                    "OIDC durable activation generation is exhausted".to_string(),
                )
            })?;
        let maximum_id_token_expires_at =
            durable_before_sign.maximum_id_token_expires_at().max(exp);
        let mut key_id_maximum_id_token_expires_at = durable_before_sign
            .key_id_maximum_id_token_expires_at()
            .clone();
        if key_id_maximum_id_token_expires_at
            .get(&signer_key_id)
            .is_some_and(|watermark| {
                watermark.canonical_public_key_identity() != signer_key_identity.as_slice()
            })
        {
            return Err(OidcError::SigningError(
                "OIDC durable key identity changed before ID-token exposure".to_string(),
            ));
        }
        let signer_expiry = key_id_maximum_id_token_expires_at
            .get(&signer_key_id)
            .map_or(exp, |watermark| watermark.expires_at().max(exp));
        key_id_maximum_id_token_expires_at.insert(
            signer_key_id,
            OidcSigningKeyExpiry::new(signer_expiry, signer_key_identity)?,
        );
        let committed = OidcSigningActivationStoreRecord::new(
            self.config.issuer.clone(),
            key_ring_generation,
            next_activation_generation,
            OidcSigningActivationStatus::Active,
            maximum_id_token_expires_at,
            key_id_maximum_id_token_expires_at.clone(),
        )?;
        let committed = dependencies.store.compare_and_set(
            cx,
            Some(durable_before_sign.activation_generation()),
            committed.clone(),
        )?;
        if committed
            != OidcSigningActivationStoreRecord::new(
                self.config.issuer.clone(),
                key_ring_generation,
                next_activation_generation,
                OidcSigningActivationStatus::Active,
                maximum_id_token_expires_at,
                key_id_maximum_id_token_expires_at,
            )?
        {
            return Err(OidcError::SigningError(
                "OIDC durable activation fence changed while signing".to_string(),
            ));
        }
        let mut slot = self.signing_activation.write().map_err(|_| {
            OidcError::SigningError("OIDC signing activation state is unavailable".to_string())
        })?;
        let active = match &mut *slot {
            OidcIdTokenSigningState::Active(active) => active,
            OidcIdTokenSigningState::Rotating { active, .. } => active,
            _ => {
                return Err(OidcError::SigningError(
                    "OIDC signing activation changed before ID-token exposure".to_string(),
                ));
            }
        };
        if active.activation_generation != durable_before_sign.activation_generation()
            || active.published.pending.binding != binding
            || active.published.pending.key_ring.generation() != key_ring_generation
        {
            return Err(OidcError::SigningError(
                "OIDC signing activation was superseded before ID-token exposure".to_string(),
            ));
        }
        active.activation_generation = committed.activation_generation();
        active.maximum_id_token_expires_at = committed.maximum_id_token_expires_at();
        active.live_key_expiries = committed.key_id_maximum_id_token_expires_at().clone();
        drop(slot);
        let raw = signed.into_compact_jws();
        verify_compact_jws_rs256(&raw, &read_back_keys).map_err(|_| {
            OidcError::SigningError("ID-token failed published-JWKS verification".to_string())
        })?;
        Ok(IdToken { raw, claims })
    }

    // -------------------------------------------------------------------------
    // UserInfo Endpoint
    // -------------------------------------------------------------------------

    /// Handles a userinfo request.
    ///
    /// Returns the user's claims filtered by the access token's scopes.
    pub fn userinfo(&self, access_token: &str) -> Result<UserClaims, OidcError> {
        let validated = self.validated_oidc_access_token(access_token)?;

        let subject = validated
            .subject
            .as_ref()
            .ok_or_else(|| OidcError::ClaimsNotFound("no subject in access token".to_string()))?;

        self.get_user_claims(subject, &validated.scopes)
    }

    fn validated_oidc_access_token(&self, access_token: &str) -> Result<OAuthToken, OidcError> {
        let validated = self
            .oauth
            .validate_access_token(access_token)
            .ok_or_else(|| {
                OidcError::OAuth(OAuthError::InvalidGrant(
                    "invalid, revoked, or expired OAuth access token".to_string(),
                ))
            })?;
        if validated.is_refresh_token || validated.is_expired() {
            return Err(OidcError::OAuth(OAuthError::InvalidGrant(
                "invalid, revoked, or expired OAuth access token".to_string(),
            )));
        }
        if !validated.scopes.iter().any(|scope| scope == "openid") {
            return Err(OidcError::MissingOpenIdScope);
        }
        Ok(validated)
    }

    // -------------------------------------------------------------------------
    // Helper Methods
    // -------------------------------------------------------------------------

    fn get_user_claims(&self, subject: &str, scopes: &[String]) -> Result<UserClaims, OidcError> {
        let provider = self
            .claims_provider
            .read()
            .ok()
            .and_then(|guard| guard.clone());

        let claims = match provider {
            Some(p) => p
                .get_claims(subject)
                .ok_or_else(|| OidcError::ClaimsNotFound(subject.to_string()))?,
            None => {
                // Default: just return subject
                UserClaims::new(subject)
            }
        };

        // The validated access token is the authority for user identity. A
        // custom provider must never be able to substitute another subject's
        // claims, even when it was queried with the correct key.
        if claims.sub != subject {
            return Err(OidcError::ClaimsSubjectMismatch);
        }

        Ok(claims.filter_by_scopes(scopes))
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

#[cfg(feature = "builtin-auth-server")]
fn validate_advertised_jwks_uri(
    issuer: &str,
    advertised_jwks_uri: &str,
) -> Result<(String, String), OidcError> {
    let issuer = Url::parse(issuer).map_err(|_| {
        OidcError::SigningError("OIDC issuer cannot bind an advertised JWKS endpoint".to_string())
    })?;
    let endpoint = Url::parse(advertised_jwks_uri).map_err(|_| {
        OidcError::SigningError("advertised JWKS endpoint is not an absolute HTTPS URL".to_string())
    })?;
    if endpoint.scheme() != "https"
        || endpoint.host_str().is_none()
        || !endpoint.username().is_empty()
        || endpoint.password().is_some()
        || endpoint.query().is_some()
        || endpoint.fragment().is_some()
    {
        return Err(OidcError::SigningError(
            "advertised JWKS endpoint is not a canonical HTTPS origin path".to_string(),
        ));
    }
    let canonical_endpoint = endpoint.to_string();
    if advertised_jwks_uri != canonical_endpoint {
        return Err(OidcError::SigningError(
            "advertised JWKS endpoint must use its exact canonical URI spelling".to_string(),
        ));
    }
    let issuer_origin = issuer.origin().ascii_serialization();
    let endpoint_origin = endpoint.origin().ascii_serialization();
    if endpoint_origin != issuer_origin {
        return Err(OidcError::SigningError(
            "advertised JWKS endpoint origin does not match the OIDC issuer".to_string(),
        ));
    }
    Ok((canonical_endpoint, endpoint_origin))
}

#[cfg(feature = "builtin-auth-server")]
fn same_id_token_authorization(left: &OAuthToken, right: &OAuthToken) -> bool {
    left.client_id == right.client_id
        && left.scopes == right.scopes
        && left.resource == right.resource
        && left.issued_at == right.issued_at
        && left.expires_at == right.expires_at
        && left.subject == right.subject
        && left.token_type == right.token_type
        && !left.is_refresh_token
        && !right.is_refresh_token
}

/// Loads and atomically reasserts the exact durable Active record before an
/// external signing dispatch. Status transitions and all generation advances
/// must change the CAS generation, so a stale process cannot overwrite a
/// retire/revoke record with its old local activation.
#[cfg(feature = "builtin-auth-server")]
fn fence_active_oidc_activation_store(
    cx: &Cx,
    dependencies: &OidcSigningActivationDependencies,
    issuer: &str,
    key_ring_generation: u64,
    activation_generation: u64,
) -> Result<OidcSigningActivationStoreRecord, OidcError> {
    let record = dependencies.store.load(cx, issuer)?.ok_or_else(|| {
        OidcError::SigningError("OIDC durable activation record is absent".to_string())
    })?;
    if record.issuer() != issuer
        || record.key_ring_generation() != key_ring_generation
        || record.activation_generation() != activation_generation
        || record.status() != OidcSigningActivationStatus::Active
    {
        return Err(OidcError::SigningError(
            "OIDC durable activation record no longer authorizes this signer".to_string(),
        ));
    }
    let fenced = dependencies.store.compare_and_set(
        cx,
        Some(record.activation_generation()),
        record.clone(),
    )?;
    if fenced != record {
        return Err(OidcError::SigningError(
            "OIDC durable activation CAS fence was lost".to_string(),
        ));
    }
    Ok(record)
}

#[cfg(feature = "builtin-auth-server")]
fn canonical_oidc_key_identity_matches(key_id: &str, identity: &[u8]) -> bool {
    let canonical = serde_json::from_slice::<serde_json::Value>(identity)
        .ok()
        .and_then(|value| serde_json::to_vec(&value).ok());
    canonical.as_deref() == Some(identity)
        && AdmittedRsaJwks::from_json(identity)
            .is_ok_and(|keys| keys.len() == 1 && keys.contains_kid(key_id))
}

#[cfg(feature = "builtin-auth-server")]
fn oidc_key_ring_public_identities(
    key_ring: &Rs256PublicKeyRing,
) -> Result<BTreeMap<String, Vec<u8>>, OidcError> {
    let mut identities = BTreeMap::new();
    for key_id in key_ring.key_ids() {
        let identity = key_ring
            .canonical_public_key_identity(&key_id)
            .ok_or_else(|| {
                OidcError::SigningError(
                    "OIDC key ring has no canonical public key identity".to_string(),
                )
            })?
            .as_bytes()
            .to_vec();
        if identities.insert(key_id, identity).is_some() {
            return Err(OidcError::SigningError(
                "OIDC key ring contains duplicate public key identifiers".to_string(),
            ));
        }
    }
    Ok(identities)
}

#[cfg(feature = "builtin-auth-server")]
fn validate_oidc_nonce(nonce: Option<&str>) -> Result<(), OidcError> {
    if nonce.is_some_and(|nonce| {
        nonce.is_empty()
            || nonce.len() > MAX_OIDC_NONCE_BYTES
            || nonce.bytes().any(|byte| byte.is_ascii_control())
    }) {
        return Err(OidcError::InvalidIdToken(
            "OIDC nonce is outside admitted bounds".to_string(),
        ));
    }
    Ok(())
}

#[cfg(feature = "builtin-auth-server")]
fn oidc_signing_canary_claims() -> Result<BoundedJwsClaims, OidcError> {
    BoundedJwsClaims::from_json_bytes(OIDC_SIGNING_CANARY_CLAIMS.as_bytes()).map_err(|_| {
        OidcError::SigningError("OIDC signing canary claims are outside signer bounds".to_string())
    })
}

#[cfg(feature = "builtin-auth-server")]
fn oidc_unix_timestamp() -> Result<i64, OidcError> {
    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| OidcError::InvalidIdToken("system clock predates Unix epoch".to_string()))?
        .as_secs();
    i64::try_from(seconds)
        .map_err(|_| OidcError::InvalidIdToken("system clock exceeds ID-token range".to_string()))
}

#[cfg(feature = "builtin-auth-server")]
fn id_token_signing_claims(claims: &IdTokenClaims) -> Result<BoundedJwsClaims, OidcError> {
    let mut value = serde_json::to_value(&claims.user_claims)
        .map_err(|_| OidcError::InvalidIdToken("user claims cannot be serialized".to_string()))?;
    let object = value.as_object_mut().ok_or_else(|| {
        OidcError::InvalidIdToken("user claims must serialize as an object".to_string())
    })?;
    // A claims provider may supply arbitrary public profile claims, but it
    // cannot overwrite the identity and lifetime facts that this server owns.
    for name in [
        "iss",
        "sub",
        "aud",
        "exp",
        "iat",
        "auth_time",
        "nonce",
        "acr",
        "amr",
        "azp",
        "at_hash",
        "c_hash",
    ] {
        object.remove(name);
    }
    object.insert(
        "iss".to_string(),
        serde_json::Value::String(claims.iss.clone()),
    );
    object.insert(
        "sub".to_string(),
        serde_json::Value::String(claims.sub.clone()),
    );
    object.insert(
        "aud".to_string(),
        serde_json::Value::String(claims.aud.clone()),
    );
    object.insert("exp".to_string(), serde_json::Value::from(claims.exp));
    object.insert("iat".to_string(), serde_json::Value::from(claims.iat));
    if let Some(auth_time) = claims.auth_time {
        object.insert("auth_time".to_string(), serde_json::Value::from(auth_time));
    }
    if let Some(nonce) = &claims.nonce {
        object.insert(
            "nonce".to_string(),
            serde_json::Value::String(nonce.clone()),
        );
    }
    if let Some(acr) = &claims.acr {
        object.insert("acr".to_string(), serde_json::Value::String(acr.clone()));
    }
    if let Some(amr) = &claims.amr {
        object.insert(
            "amr".to_string(),
            serde_json::to_value(amr).map_err(|_| {
                OidcError::InvalidIdToken("authentication methods cannot be serialized".to_string())
            })?,
        );
    }
    if let Some(azp) = &claims.azp {
        object.insert("azp".to_string(), serde_json::Value::String(azp.clone()));
    }
    if let Some(at_hash) = &claims.at_hash {
        object.insert(
            "at_hash".to_string(),
            serde_json::Value::String(at_hash.clone()),
        );
    }
    if let Some(c_hash) = &claims.c_hash {
        object.insert(
            "c_hash".to_string(),
            serde_json::Value::String(c_hash.clone()),
        );
    }
    BoundedJwsClaims::from_value(&value)
        .map_err(|_| OidcError::InvalidIdToken("ID-token claims exceed signing bounds".to_string()))
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod non_signing_tests {
    use super::*;

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
            .unwrap(),
        );
        let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).unwrap();
        assert_eq!(provider.config().issuer, oauth.config().issuer);
        assert_eq!(
            provider.discovery_document("https://issuer.example").issuer,
            oauth.config().issuer
        );

        assert!(matches!(
            OidcProvider::new(Arc::clone(&oauth), OidcProviderConfig::default()),
            Err(OidcError::OAuth(OAuthError::ServerError(_)))
        ));

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
        assert!(doc.authorization_response_iss_parameter_supported);
        assert!(doc.token_endpoint_auth_methods_supported.iter().any(|method| method == "none"));
        assert!(doc.revocation_endpoint_auth_methods_supported.iter().any(|method| method == "none"));
        let wire = serde_json::to_value(&doc).unwrap();
        assert_eq!(wire["authorization_response_iss_parameter_supported"], true);
        assert_eq!(wire["revocation_endpoint_auth_methods_supported"][0], "none");
    }

    #[test]
    fn discovery_auth_methods_follow_public_client_policy() {
        for (allow_public_clients, expects_none) in [(true, true), (false, false)] {
            let oauth = Arc::new(OAuthServer::try_new(OAuthServerConfig {
                issuer: "https://issuer.example".to_string(),
                allow_public_clients,
                ..OAuthServerConfig::default()
            }).unwrap());
            let provider = OidcProvider::with_defaults(oauth).unwrap();
            let doc = provider.discovery_document("https://issuer.example");
            assert_eq!(
                doc.token_endpoint_auth_methods_supported.iter().any(|method| method == "none"),
                expects_none,
            );
            assert_eq!(
                doc.revocation_endpoint_auth_methods_supported.iter().any(|method| method == "none"),
                expects_none,
            );
            assert!(doc.token_endpoint_auth_methods_supported.iter().any(|method| method == "client_secret_basic"));
            assert!(doc.revocation_endpoint_auth_methods_supported.iter().any(|method| method == "client_secret_basic"));
            assert!(doc.authorization_response_iss_parameter_supported);
            assert_eq!(doc.registration_endpoint.is_some(), expects_none);
            if let Some(endpoint) = &doc.registration_endpoint {
                assert_eq!(endpoint, "https://issuer.example/register");
            }
        }
    }

    #[test]
    fn discovery_advertises_the_served_routes_for_either_base_spelling() {
        use crate::oauth::OAuthHttpRoutes;

        // The two bases differ only in a trailing slash; both serve `/token`.
        for base in ["https://issuer.example", "https://issuer.example/"] {
            let oauth = Arc::new(
                OAuthServer::try_new(OAuthServerConfig {
                    issuer: base.to_string(),
                    allow_public_clients: true,
                    ..OAuthServerConfig::default()
                })
                .unwrap(),
            );
            let routes = OAuthHttpRoutes::new(Arc::clone(&oauth), base).unwrap();
            let doc = OidcProvider::with_defaults(oauth)
                .unwrap()
                .discovery_document(routes.public_endpoint_base());
            let served = |path: &str| format!("https://issuer.example{path}");
            assert_eq!(
                doc.authorization_endpoint,
                served(routes.authorization_path()),
                "{base}"
            );
            assert_eq!(doc.token_endpoint, served(routes.token_path()), "{base}");
            assert_eq!(
                doc.revocation_endpoint,
                Some(served(routes.revocation_path())),
                "{base}"
            );
            assert_eq!(
                doc.registration_endpoint,
                routes.registration_path().map(served),
                "{base}"
            );
        }
    }

    /// FND-01 surface 2, frozen case 4.
    ///
    /// **This case proves the absence of an advertised issuance path. It does
    /// not prove the runtime refusal.** The two halves are deliberately
    /// separate cases so a reader can tell which one carries the security
    /// claim; see `signer_activation_tests::
    /// issuance_without_activation_fails_closed_with_signing_error` for the
    /// half that does.
    ///
    /// It is ungated because the frozen target map pins this case to
    /// `oidc::non_signing_tests` on the `fastmcp-server` lib target, and that
    /// module compiles under the crate's default features
    /// (`default = ["legacy-2024-11-05"]`). Under those features
    /// [`OidcProvider::issue_id_token`] does not exist at all — it is
    /// `#[cfg(feature = "builtin-auth-server")]` — so there is no issuance
    /// entry point here to refuse. That absence is a compile-time fact and
    /// cannot be asserted at run time; what is asserted is its externally
    /// observable consequence.
    ///
    /// What is proved: a provider that has activated no external signer
    /// advertises no ID-token signing capability, so no relying party can be
    /// induced to expect a signed ID token. This holds in *both* feature
    /// configurations, because `discovery_document` populates
    /// `id_token_signing_alg_values_supported` and `jwks_uri` only when an
    /// activation is both active and published. The case therefore means the
    /// same thing however the crate is built.
    ///
    /// This is distinct from `discovery_does_not_advertise_signing`, which
    /// asserts the defaults of a bare [`DiscoveryDocument`]. This asserts what
    /// a live provider actually advertises.
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
    fn claims_provider_cannot_substitute_a_different_subject() {
        let oauth = Arc::new(OAuthServer::new(OAuthServerConfig::default()));
        let provider = OidcProvider::with_defaults(oauth).expect("default provider");
        provider.set_claims_fn(|requested_subject| {
            assert_eq!(requested_subject, "authenticated-subject");
            Some(UserClaims::new("different-subject").with_email("different-subject@example.test"))
        });

        let error = provider
            .get_user_claims(
                "authenticated-subject",
                &["openid".to_string(), "email".to_string()],
            )
            .expect_err("a claims provider must not substitute another identity");

        assert!(matches!(error, OidcError::ClaimsSubjectMismatch));
        assert!(!error.to_string().contains("authenticated-subject"));
        assert!(!error.to_string().contains("different-subject"));
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

        let wire = serde_json::to_value(&id_token_claims).unwrap();
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
}

#[cfg(all(test, feature = "builtin-auth-server"))]
mod signer_activation_tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Mutex, Weak};

    use base64::Engine as _;
    use fastmcp_protocol::jose::{
        AttestedRs256PublicKey, ExternalRs256OperationReceipt, ExternalRs256SignDisposition,
        ExternalRs256SignerBackend, ExternalRs256SigningRequest, RawRs256Signature,
        RedactedSignerProvenance,
    };
    use ring::rand::SystemRandom;
    use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    use super::*;
    use crate::oauth::{AuthorizationRequest, CodeChallengeMethod, OAuthClient, TokenRequest};

    const TEST_CLIENT_ID: &str = "oidc-signing-test-client";
    const TEST_REDIRECT_URI: &str = "http://127.0.0.1/oidc-callback";
    const TEST_CODE_VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
    const TEST_CODE_CHALLENGE: &str = "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM";
    // This is a retained public RS256 verification vector; no private key is
    // present here. Negative tests never ask their backend to produce a JWS.
    const TEST_PUBLIC_MODULUS: &str = "jlHZ9nzuIuM4aiAQSAgEJMBaYS7qm7Z_3mtGYDdzReIkzxPHHr21oeXQyUJI89eQG13fsUdyoodcuh5kmndPCrODJekfr_zgor6sNspcB88iQEqEc9yf9YAf5v-cNH1Evh82KABuWb26LMaNAzZFR3BMhMEQ1FD6fLFGAbX76Drd5_UZ-1xcU07IXEc_9zvQvOwXckhO7P5Yil1fVzLTrHye_6zTbGWvdqi45095bKPnSqjrLBCTVrUW8o02Gi6mt7Ls9pZeWx2DXV8SqV06DdlqiovtKWRooQ1zV-v7BGsLsVk6T6d-8mNMGNrh0fpNb_5kdaHphAt_Ji6eE1wQPw";
    const TEST_CANARY_COMPACT_JWS: &str = concat!(
        "eyJhbGciOiJSUzI1NiIsImtpZCI6ImZpeGVkLXJzMjU2In0.",
        "eyJzdWIiOiJmaXhlZC12ZWN0b3IiLCJhdWQiOiJzZXJ2ZXItcG9saWN5LWxhdGVyIn0.",
        "Oak9UDEtrL-pNcPIFw31uzuCoCTyXywF5i3jxDixd0gHonZYPFfSlyPwhNSTrqmlzPsL-wNFcDn1zFlug6Ae1vK_QaL-bZBSxq-lOrMDUI_5_3P_HUrngtZaNk8ru88-wdGByGm1jRZa-LfeoSkESHVKPIcQ_WT7wqhq1RX3ZrPiq9QkHFE8nWIgiIesu8DFOXsdN05rmOxHheCbDGRpf8cQAG0ZENpJvYugD-SX9Sg9Kds5HOlOt6csIQBexCeKM2rIrN0r7qCp6jx_0aevqU6rNr6oxCxCGoH3UZGJa5xRh2KeJ6NVBE9BpPW3Kdi3dEfKlKldjzlUW-zEREdeEw"
    );
    // Test-only PKCS#8 fixture for a simulated externally-custodied signer.
    // This module alone is cfg(test); production accepts only an embedding
    // supplied `ExternalRs256SignerBackend` and contains no private key path.
    const TEST_DYNAMIC_PRIVATE_KEY_PKCS8_B64: &str = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQC2H073wfexUwbdJyzIgK9KMxPV7ocei0R/RIRJNKyENR9JlymRa1MJtEuHmR5gQaVB5UkHZTlumKxFvt5ahOWKVQYCi8Qmu9bb0MjcWGP7kasqmapZqIa0eZQX7ax4wk0i2sljuQHrIdZpfBwS2ZMpx9GoKlvLSyuC+XtpapLf3Rfz5o0PAT0Iq9QGtoKFeFGV048OTTVN+Kianun2MHy3qqAS+PXMS6Ld5DRHpGbbyvGNXXtVC6/QyR0nf5hCbKFd7i6oEYPUzaxXMkaSvAIQJ6hvcH7mkXJ9EDewTdhuVVnf6IxpRX+3w4mP0hwVXM62PHSgOjxYJoy1F1c1zBHpAgMBAAECggEAQsfCLgka6RO3vZoiyradEAkgqd5X/3QzvrMCCtHcvHG6MkLShDclcLaCx4r233bSwRHxMFwGri4fZUeywuBeRtYcaQyU9VsFUv8A2AM1TkbAy9Mi6tNn6X93NTr6diFRJYmyNPXe5Tg4Jd/Tb3oGg1h44T/+/tFmeBVBEot4pcsOqk/IeKaaEsbodSBH3bAHSuq/Pp6vwWSuzEAQoCa/SSvOJ8vwNX6xw0QDvR6PlMkfR1J5JM0zdhgZFBeWL1lTu5kJsH49ytAceCMlR/EodDSVIpYEiPJBggVFMoc3rkqeu6J0nS/TxBwyXDc/GVJn1uPna+53uV+s5Ljw39GErQKBgQDpfHnTE3tb8ph27dJ3OF1h2VjqVxB85OpXhOxhSzIo6qyZuqvt7hBVZTKo8BHWFkjK9/NzoC1MPcgUJLiBdqoI+YSnYOKkEDscRWFWQu4gSj4tmxYrvpF+HEaNw/khvcKtn8uIYKTa5Vp/5eMKT04fjthyuToIufX+6IEhZsjcdwKBgQDHru6m0u9Cv3/qDl5W7+oq7KP0I9KwHx690dcABVN+ICiXH0ZfY0QkTGP1ihtR794VqX9fofd+vGqlcl7WNULVh7J0hfYffjNP54yuW9DnM8OwuIPhamFcJST9IpphpGS6AuOaxrElS/BmM4J7IrCNqyt600MbBaV87m7Mear8nwKBgQDKXOE1aSA0rAkqoqsUO1zsLrWavYUDyl+1JPa+yK6bufGId7sFx5yOdtw2gYPj+oJyr/5ny38XIkDj/IORaairiJ9JdnZYXdztftCDNBUxFUfYvR61IUD2fUlFG4I0lURCuUltVN3s/nW2fieOSvfZ8DN3E0TSRWKI4TjyGyShtwKBgAjO+7IaPfm4zuC5T4oQPUk1dSoQ5ntkdAu1lQFoOr2ml4PLGmSc0WW0hPhQ5lGf/jEAcCD82RkbIK05tVtHsDIRMVsYibnr7EGLGlaasEVysCA8k3y/H5pb/Ry5iQvjn5nhBL9QIoJdrjYj8Y6TAizNrzZU2XH4tssjDXoxp8xLAoGARgFG1+rEzvAN+cBcYqBahxte52q6oZIB/haRnVx47O3lqBoKMsE5KQq4N1kiP14Ge+WmEs2MRXe9mZ1PNBFRZS/0+raba3yuz78SjQPo9l4MSxV53bAK73QFzaWicHPSeONcxyq8AXvSebcuIfZWGrmhImRnAtc3LZH/5aVWIrk=";
    const TEST_DYNAMIC_PUBLIC_MODULUS: &str = "th9O98H3sVMG3ScsyICvSjMT1e6HHotEf0SESTSshDUfSZcpkWtTCbRLh5keYEGlQeVJB2U5bpisRb7eWoTlilUGAovEJrvW29DI3Fhj-5GrKpmqWaiGtHmUF-2seMJNItrJY7kB6yHWaXwcEtmTKcfRqCpby0srgvl7aWqS390X8-aNDwE9CKvUBraChXhRldOPDk01Tfiomp7p9jB8t6qgEvj1zEui3eQ0R6Rm28rxjV17VQuv0MkdJ3-YQmyhXe4uqBGD1M2sVzJGkrwCECeob3B-5pFyfRA3sE3YblVZ3-iMaUV_t8OJj9IcFVzOtjx0oDo8WCaMtRdXNcwR6Q";

    struct TestOnlyDynamicRs256Backend {
        calls: Arc<AtomicUsize>,
        signing_inputs: Mutex<Vec<Vec<u8>>>,
    }

    impl ExternalRs256SignerBackend for TestOnlyDynamicRs256Backend {
        fn sign<'a>(
            &'a self,
            _: &'a Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            Box::pin(async move {
                let signing_input = request.input().with_bytes(|input| input.to_vec());
                self.signing_inputs
                    .lock()
                    .expect("dynamic signer input capture lock")
                    .push(signing_input.clone());
                let private_key = base64::engine::general_purpose::STANDARD
                    .decode(TEST_DYNAMIC_PRIVATE_KEY_PKCS8_B64)
                    .expect("bounded test-only PKCS#8 fixture");
                let key_pair = RsaKeyPair::from_pkcs8(&private_key)
                    .expect("valid test-only external-custody key");
                let mut signature = vec![0_u8; key_pair.public().modulus_len()];
                key_pair
                    .sign(
                        &RSA_PKCS1_SHA256,
                        &SystemRandom::new(),
                        &signing_input,
                        &mut signature,
                    )
                    .expect("real RS256 test signing succeeds");
                let operation = self.calls.fetch_add(1, Ordering::AcqRel) + 1;
                let receipt = ExternalRs256OperationReceipt::new(
                    request.binding(),
                    u64::try_from(operation).expect("bounded test operation count"),
                    RedactedSignerProvenance::new("oidc-test-external-custody")
                        .expect("bounded redacted test provenance"),
                )
                .expect("valid dynamic test signing receipt");
                ExternalRs256SignDisposition::Dispatched(
                    RawRs256Signature::from_bytes(signature)
                        .expect("real RS256 fixture width is admitted"),
                    receipt,
                )
            })
        }
    }

    struct FixedCanaryBackend {
        calls: Arc<AtomicUsize>,
    }

    fn fixed_canary_disposition(
        request: ExternalRs256SigningRequest,
    ) -> ExternalRs256SignDisposition {
        let (input, signature) = TEST_CANARY_COMPACT_JWS
            .rsplit_once('.')
            .expect("retained test canary has signature");
        assert!(
            request
                .input()
                .with_bytes(|bytes| bytes == input.as_bytes())
        );
        let signature = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(signature)
            .expect("retained public canary signature");
        let receipt = ExternalRs256OperationReceipt::new(
            request.binding(),
            1,
            RedactedSignerProvenance::new("oidc-public-canary-test")
                .expect("bounded redacted test provenance"),
        )
        .expect("valid dispatched-operation receipt");
        ExternalRs256SignDisposition::Dispatched(
            RawRs256Signature::from_bytes(signature).expect("retained RS256 signature length"),
            receipt,
        )
    }

    impl ExternalRs256SignerBackend for FixedCanaryBackend {
        fn sign<'a>(
            &'a self,
            _: &'a Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::AcqRel);
                fixed_canary_disposition(request)
            })
        }
    }

    struct StaleReadBackBackend {
        calls: Arc<AtomicUsize>,
        provider: Arc<Mutex<Option<Weak<OidcProvider>>>>,
    }

    impl ExternalRs256SignerBackend for StaleReadBackBackend {
        fn sign<'a>(
            &'a self,
            _: &'a Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let calls = Arc::clone(&self.calls);
            let provider = Arc::clone(&self.provider);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::AcqRel);
                let provider = provider
                    .lock()
                    .expect("stale read-back test provider lock")
                    .as_ref()
                    .and_then(Weak::upgrade)
                    .expect("activation retains stale read-back test provider");
                assert!(
                    provider
                        .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                        .is_ok()
                );
                fixed_canary_disposition(request)
            })
        }
    }

    struct UnexpectedBackend {
        calls: Arc<AtomicUsize>,
    }

    /// Test-only external verifier.  It models an embedding-owned observation
    /// boundary; production native HTTP tests use a loopback implementation.
    struct FixedReadBackVerifier {
        bytes: Vec<u8>,
        generation: u64,
        origin: &'static str,
    }

    impl OidcJwksReadBackVerifier for FixedReadBackVerifier {
        fn read_back<'a>(
            &'a self,
            _: &'a Cx,
            endpoints: &'a [String],
        ) -> Pin<Box<dyn Future<Output = Result<Vec<JwksEndpointReadBack>, OidcError>> + Send + 'a>>
        {
            let bytes = self.bytes.clone();
            let generation = self.generation;
            Box::pin(async move {
                endpoints
                    .iter()
                    .map(|uri| {
                        JwksEndpointReadBack::new(
                            uri.clone(),
                            self.origin,
                            bytes.clone(),
                            generation,
                        )
                        .map_err(|_| {
                            OidcError::SigningError("test verifier evidence failed".to_string())
                        })
                    })
                    .collect()
            })
        }
    }

    struct MutableReadBackVerifier {
        evidence: Mutex<(Vec<u8>, u64, &'static str)>,
    }

    impl MutableReadBackVerifier {
        fn set(&self, bytes: Vec<u8>, generation: u64) {
            *self
                .evidence
                .lock()
                .expect("mutable verifier evidence lock") =
                (bytes, generation, "https://fastmcp.invalid");
        }
    }

    impl OidcJwksReadBackVerifier for MutableReadBackVerifier {
        fn read_back<'a>(
            &'a self,
            _: &'a Cx,
            endpoints: &'a [String],
        ) -> Pin<Box<dyn Future<Output = Result<Vec<JwksEndpointReadBack>, OidcError>> + Send + 'a>>
        {
            let (bytes, generation, origin) = self
                .evidence
                .lock()
                .expect("mutable verifier evidence lock")
                .clone();
            Box::pin(async move {
                endpoints
                    .iter()
                    .map(|uri| {
                        JwksEndpointReadBack::new(uri.clone(), origin, bytes.clone(), generation)
                            .map_err(|_| {
                                OidcError::SigningError(
                                    "mutable verifier evidence failed".to_string(),
                                )
                            })
                    })
                    .collect()
            })
        }
    }

    /// In-repo durable-store contract fixture: two providers can share one
    /// store across simulated process replacement and exercise CAS fencing.
    #[derive(Default)]
    struct TestDurableActivationStore {
        record: Mutex<Option<OidcSigningActivationStoreRecord>>,
    }

    impl OidcSigningActivationStore for TestDurableActivationStore {
        fn load(
            &self,
            cx: &Cx,
            issuer: &str,
        ) -> Result<Option<OidcSigningActivationStoreRecord>, OidcError> {
            cx.checkpoint().map_err(|_| {
                OidcError::SigningError("test activation store cancelled".to_string())
            })?;
            Ok(self
                .record
                .lock()
                .map_err(|_| {
                    OidcError::SigningError("test activation store unavailable".to_string())
                })?
                .clone()
                .filter(|record| record.issuer() == issuer))
        }

        fn compare_and_set(
            &self,
            cx: &Cx,
            expected_generation: Option<u64>,
            next: OidcSigningActivationStoreRecord,
        ) -> Result<OidcSigningActivationStoreRecord, OidcError> {
            cx.checkpoint().map_err(|_| {
                OidcError::SigningError("test activation store cancelled".to_string())
            })?;
            let mut record = self.record.lock().map_err(|_| {
                OidcError::SigningError("test activation store unavailable".to_string())
            })?;
            if record
                .as_ref()
                .map(OidcSigningActivationStoreRecord::activation_generation)
                != expected_generation
            {
                return Err(OidcError::SigningError(
                    "test activation CAS lost".to_string(),
                ));
            }
            *record = Some(next.clone());
            Ok(next)
        }
    }

    impl ExternalRs256SignerBackend for UnexpectedBackend {
        fn sign<'a>(
            &'a self,
            _: &'a Cx,
            _: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::AcqRel);
                panic!("a rejected OIDC issuance must not dispatch external signing")
            })
        }
    }

    struct CancellationAfterDispatchBackend {
        calls: Arc<AtomicUsize>,
    }

    impl ExternalRs256SignerBackend for CancellationAfterDispatchBackend {
        fn sign<'a>(
            &'a self,
            cx: &'a Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::AcqRel);
                cx.set_cancel_requested(true);
                let receipt = ExternalRs256OperationReceipt::new(
                    request.binding(),
                    1,
                    RedactedSignerProvenance::new("oidc-cancellation-test")
                        .expect("bounded redacted test provenance"),
                )
                .expect("valid dispatched-operation receipt");
                // This is intentionally not a signature: cancellation must
                // consume the dispatched attempt before any JWS can exist.
                ExternalRs256SignDisposition::Dispatched(
                    RawRs256Signature::from_bytes(vec![0_u8; 256])
                        .expect("bounded cancellation-path bytes"),
                    receipt,
                )
            })
        }
    }

    struct CancelIdTokenAfterSigningBackend {
        calls: Arc<AtomicUsize>,
    }

    impl ExternalRs256SignerBackend for CancelIdTokenAfterSigningBackend {
        fn sign<'a>(
            &'a self,
            cx: &'a Cx,
            request: ExternalRs256SigningRequest,
        ) -> Pin<Box<dyn Future<Output = ExternalRs256SignDisposition> + Send + 'a>> {
            let calls = Arc::clone(&self.calls);
            Box::pin(async move {
                calls.fetch_add(1, Ordering::AcqRel);
                let is_canary = request.input().with_bytes(|input| {
                    TEST_CANARY_COMPACT_JWS
                        .rsplit_once('.')
                        .is_some_and(|(canary_input, _)| input == canary_input.as_bytes())
                });
                if is_canary {
                    return fixed_canary_disposition(request);
                }
                cx.set_cancel_requested(true);
                let receipt = ExternalRs256OperationReceipt::new(
                    request.binding(),
                    2,
                    RedactedSignerProvenance::new("oidc-id-token-cancel-test")
                        .expect("bounded redacted test provenance"),
                )
                .expect("valid dispatched-operation receipt");
                ExternalRs256SignDisposition::Dispatched(
                    RawRs256Signature::from_bytes(vec![0_u8; 256])
                        .expect("bounded cancellation-path bytes"),
                    receipt,
                )
            })
        }
    }

    fn test_signer_with_binding(
        backend: Arc<dyn ExternalRs256SignerBackend>,
        binding: Rs256SigningBinding,
    ) -> Arc<ExternalRs256Signer> {
        test_signer_with_kid_and_binding(backend, "fixed-rs256", binding)
    }

    fn test_signer_with_kid_and_binding(
        backend: Arc<dyn ExternalRs256SignerBackend>,
        kid: &str,
        binding: Rs256SigningBinding,
    ) -> Arc<ExternalRs256Signer> {
        let modulus = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(TEST_PUBLIC_MODULUS)
            .expect("retained public verification modulus");
        let key = AttestedRs256PublicKey::admit(
            kid,
            modulus,
            binding,
            RedactedSignerProvenance::new("oidc-test-adapter")
                .expect("bounded redacted test provenance"),
        )
        .expect("retained public verification key admits");
        Arc::new(ExternalRs256Signer::new(backend, key))
    }

    fn test_signer(backend: Arc<dyn ExternalRs256SignerBackend>) -> Arc<ExternalRs256Signer> {
        let binding =
            Rs256SigningBinding::new(11, 12, 13, 14).expect("nonzero external signer generations");
        test_signer_with_binding(backend, binding)
    }

    fn dynamic_test_signer(
        backend: Arc<dyn ExternalRs256SignerBackend>,
    ) -> Arc<ExternalRs256Signer> {
        dynamic_test_signer_with_kid(
            backend,
            "dynamic-external-rs256",
            Rs256SigningBinding::new(21, 22, 23, 24).expect("nonzero dynamic signer generations"),
        )
    }

    fn dynamic_test_signer_with_kid(
        backend: Arc<dyn ExternalRs256SignerBackend>,
        kid: &str,
        binding: Rs256SigningBinding,
    ) -> Arc<ExternalRs256Signer> {
        let modulus = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(TEST_DYNAMIC_PUBLIC_MODULUS)
            .expect("test-only external-custody public modulus");
        let key = AttestedRs256PublicKey::admit(
            kid,
            modulus,
            binding,
            RedactedSignerProvenance::new("oidc-dynamic-test-adapter")
                .expect("bounded redacted test provenance"),
        )
        .expect("test-only external-custody key admits");
        Arc::new(ExternalRs256Signer::new(backend, key))
    }

    fn issue_access_token(scopes: &[&str]) -> (Arc<OAuthServer>, crate::oauth::TokenResponse) {
        let oauth = Arc::new(OAuthServer::with_defaults());
        oauth
            .register_client(
                OAuthClient::builder(TEST_CLIENT_ID)
                    .redirect_uri(TEST_REDIRECT_URI)
                    .scopes(scopes.iter().copied())
                    .build()
                    .expect("valid test client"),
            )
            .expect("register test client");
        let (code, _) = oauth
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: TEST_CLIENT_ID.to_string(),
                redirect_uri: TEST_REDIRECT_URI.to_string(),
                scopes: scopes.iter().map(|scope| (*scope).to_string()).collect(),
                resource: None,
                state: Some("oidc-test-state".to_string()),
                code_challenge: TEST_CODE_CHALLENGE.to_string(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .expect("authorize test access token");
        let response = oauth
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                redirect_uri: Some(TEST_REDIRECT_URI.to_string()),
                client_id: TEST_CLIENT_ID.to_string(),
                client_secret: None,
                code_verifier: Some(TEST_CODE_VERIFIER.to_string()),
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .expect("exchange test access token");
        (oauth, response)
    }

    #[test]
    fn durable_key_expiry_record_is_publicly_constructible_and_bounded() {
        let signer = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let watermark = OidcSigningKeyExpiry::new(
            99,
            signer
                .canonical_public_jwks()
                .expect("canonical public identity")
                .as_bytes()
                .to_vec(),
        )
        .expect("bounded public watermark");
        let record = OidcSigningActivationStoreRecord::new(
            "https://fastmcp.invalid/",
            1,
            1,
            OidcSigningActivationStatus::Active,
            99,
            BTreeMap::from([("fixed-rs256".to_string(), watermark.clone())]),
        )
        .expect("embedding-owned durable store can reconstruct a record");
        assert_eq!(
            record
                .key_id_maximum_id_token_expires_at()
                .get("fixed-rs256")
                .map(OidcSigningKeyExpiry::expires_at),
            Some(99),
        );

        let too_many = (0..=fastmcp_protocol::jose::MAX_JWKS_KEYS)
            .map(|index| (format!("key-{index}"), watermark.clone()))
            .collect();
        assert!(
            OidcSigningActivationStoreRecord::new(
                "https://fastmcp.invalid/",
                1,
                1,
                OidcSigningActivationStatus::Active,
                99,
                too_many,
            )
            .is_err()
        );
    }

    fn provider_with_published_jwks(
        oauth: Arc<OAuthServer>,
        signer: Arc<ExternalRs256Signer>,
    ) -> OidcProvider {
        provider_with_published_jwks_observed_generation(oauth, signer, None)
    }

    fn provider_with_published_jwks_observed_generation(
        oauth: Arc<OAuthServer>,
        signer: Arc<ExternalRs256Signer>,
        observed_generation: Option<u64>,
    ) -> OidcProvider {
        let signer_ring_generation = signer.binding().ring_generation();
        let key_ring = Rs256PublicKeyRing::new(signer, Vec::new(), signer_ring_generation)
            .expect("single signer test key ring");
        provider_with_published_key_ring_observed_generation(oauth, key_ring, observed_generation)
    }

    fn provider_with_published_key_ring_observed_generation(
        oauth: Arc<OAuthServer>,
        key_ring: Rs256PublicKeyRing,
        observed_generation: Option<u64>,
    ) -> OidcProvider {
        let provider = OidcProvider::with_defaults(oauth).expect("OIDC provider");
        let canonical = key_ring
            .canonical_public_jwks()
            .expect("canonical external public JWKS");
        provider
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: canonical.as_bytes().to_vec(),
                    generation: observed_generation.unwrap_or(key_ring.generation()),
                    origin: "https://fastmcp.invalid",
                }),
                Arc::new(TestDurableActivationStore::default()),
            )
            .expect("install external verifier and durable activation store");
        provider
            .begin_id_token_signing_key_ring_activation(
                key_ring,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            )
            .expect("begin OIDC signer publication");
        provider
            .publish_id_token_signing_key_ring_jwks(canonical)
            .expect("publish exact canonical JWKS");
        provider
    }

    fn activate_provider(provider: &OidcProvider) -> Result<(), OidcError> {
        assert!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .is_ok()
        );
        let cx = Cx::for_testing();
        fastmcp_core::block_on(provider.activate_id_token_signing(&cx, signing_deadline()))
    }

    fn signing_deadline() -> ExternalRs256SigningDeadline {
        ExternalRs256SigningDeadline::new(std::time::Duration::from_secs(1))
            .expect("bounded test deadline")
    }

    /// FND-01 surface 2, the security half of frozen case 4.
    ///
    /// **This is the case that proves the fail-closed security claim.** Its
    /// ungated sibling, `non_signing_tests::
    /// issuance_fails_closed_without_external_signer`, proves only that an
    /// unactivated provider advertises no signing capability; it cannot reach
    /// this refusal because [`OidcProvider::issue_id_token`] does not exist
    /// under the crate's default features.
    ///
    /// Surface identity, stated explicitly because the feature is part of it:
    /// package `fastmcp-server`, lib target, module
    /// `oidc::signer_activation_tests`, cfg
    /// `all(test, feature = "builtin-auth-server")`. The behaviour under test
    /// does not exist without that feature, so this case is a different
    /// surface from its ungated sibling rather than a relocation of it.
    ///
    /// What is proved: a provider that has activated no external signer
    /// refuses to issue an ID token, and refuses *at the signing-activation
    /// boundary*. The boundary assertion is the point of the case — reaching
    /// an access-token or claims refusal first would leave the fail-closed
    /// property unproven while the test still passed green.
    #[test]
    fn issuance_without_activation_fails_closed_with_signing_error() {
        let (oauth, issued) = issue_access_token(&["openid"]);
        // Deliberately `with_defaults`: no signer is begun, published, or
        // activated, so the activation slot is empty at issuance time.
        let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("default provider");
        let cx = Cx::for_testing();

        let error = fastmcp_core::block_on(provider.issue_id_token(
            &cx,
            &issued.access_token,
            None,
            signing_deadline(),
        ))
        .expect_err("issuance must fail closed without an activated external signer");

        // The credential itself is valid and openid-scoped, and a default
        // provider's claims lookup succeeds, so the only refusal this call can
        // reach is the signing-activation one. Asserting the exact boundary
        // keeps the case from passing for an unrelated reason.
        assert!(
            matches!(&error, OidcError::SigningError(message) if message.contains("activation")),
            "expected the signing-activation refusal, saw {error:?}"
        );
    }

    #[test]
    fn forged_revoked_refresh_and_non_openid_credentials_never_dispatch_signing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::clone(&calls),
        }));
        let (oauth, issued) = issue_access_token(&["openid"]);
        let refresh = issued
            .refresh_token
            .as_deref()
            .expect("refresh credential")
            .to_string();
        let provider = provider_with_published_jwks(Arc::clone(&oauth), signer);
        let cx = Cx::for_testing();
        let refresh_result = fastmcp_core::block_on(provider.issue_id_token(
            &cx,
            &refresh,
            None,
            signing_deadline(),
        ));
        assert!(matches!(
            refresh_result,
            Err(OidcError::OAuth(OAuthError::InvalidGrant(_)))
        ));
        oauth
            .revoke(&issued.access_token, TEST_CLIENT_ID, None)
            .expect("revoke owned access credential");

        for credential in ["forged-opaque-credential".to_string(), issued.access_token] {
            let cx = Cx::for_testing();
            let result = fastmcp_core::block_on(provider.issue_id_token(
                &cx,
                &credential,
                None,
                signing_deadline(),
            ));
            assert!(matches!(
                result,
                Err(OidcError::OAuth(OAuthError::InvalidGrant(_)))
            ));
        }
        assert_eq!(calls.load(Ordering::Acquire), 0);

        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::clone(&calls),
        }));
        let (oauth, issued) = issue_access_token(&[]);
        let provider = provider_with_published_jwks(oauth, signer);
        let cx = Cx::for_testing();
        let result = fastmcp_core::block_on(provider.issue_id_token(
            &cx,
            &issued.access_token,
            None,
            signing_deadline(),
        ));
        assert!(matches!(result, Err(OidcError::MissingOpenIdScope)));
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rh5_one_field_wrong_read_back_origin_cannot_commit_activation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&calls),
        }));
        let store = Arc::new(TestDurableActivationStore::default());
        let provider = OidcProvider::with_defaults(Arc::new(OAuthServer::with_defaults()))
            .expect("OIDC provider");
        let canonical = signer
            .canonical_public_jwks()
            .expect("canonical external public JWKS");
        provider
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: canonical.as_bytes().to_vec(),
                    generation: signer.binding().ring_generation(),
                    // Only this receipt field differs from the configured
                    // endpoint: the verifier attests a sibling origin.
                    origin: "https://fastmcp.invalid.evil",
                }),
                store.clone(),
            )
            .expect("install test verifier and durable store");
        provider
            .begin_id_token_signing_activation(signer, "https://fastmcp.invalid/oidc/jwks")
            .expect("begin pending activation");
        provider
            .publish_id_token_signing_jwks(canonical)
            .expect("publish exact canonical JWKS");
        assert!(matches!(
            fastmcp_core::block_on(
                provider.activate_id_token_signing(&Cx::for_testing(), signing_deadline(),)
            ),
            Err(OidcError::SigningError(_))
        ));
        assert!(provider.activated_jwks_document().is_err());
        assert!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("durable store remains readable")
                .is_none()
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn rh5_no_publish_never_serves_or_activates_a_pending_signer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::clone(&calls),
        }));
        let provider = OidcProvider::with_defaults(Arc::new(OAuthServer::with_defaults()))
            .expect("OIDC provider");
        provider
            .begin_id_token_signing_activation(signer, "https://fastmcp.invalid/oidc/jwks")
            .expect("begin Pending activation");

        assert!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .is_err()
        );
        assert!(matches!(
            fastmcp_core::block_on(
                provider.activate_id_token_signing(&Cx::for_testing(), signing_deadline(),)
            ),
            Err(OidcError::SigningError(_))
        ));
        assert!(provider.activated_jwks_document().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rh5_stale_signer_generation_cannot_publish_or_activate() {
        let calls = Arc::new(AtomicUsize::new(0));
        let pending_signer = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::clone(&calls),
        }));
        let stale_signer = test_signer_with_binding(
            Arc::new(UnexpectedBackend {
                calls: Arc::clone(&calls),
            }),
            Rs256SigningBinding::new(11, 12, 14, 14).expect("different well-formed key generation"),
        );
        let provider = OidcProvider::with_defaults(Arc::new(OAuthServer::with_defaults()))
            .expect("OIDC provider");
        provider
            .begin_id_token_signing_activation(pending_signer, "https://fastmcp.invalid/oidc/jwks")
            .expect("begin pending activation");
        let stale_canonical = stale_signer
            .canonical_public_jwks()
            .expect("stale signer canonical JWKS");
        assert!(matches!(
            provider.publish_id_token_signing_jwks(stale_canonical),
            Err(OidcError::SigningError(_))
        ));
        assert!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .is_err()
        );
        assert!(provider.activated_jwks_document().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 0);
    }

    #[test]
    fn rh5_one_field_stale_read_back_generation_cannot_commit_durable_activation() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&calls),
        }));
        let canonical = signer
            .canonical_public_jwks()
            .expect("canonical public JWKS");
        let store = Arc::new(TestDurableActivationStore::default());
        let provider = OidcProvider::with_defaults(Arc::new(OAuthServer::with_defaults()))
            .expect("OIDC provider");
        provider
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: canonical.as_bytes().to_vec(),
                    // This is the only changed receipt field: the advertised
                    // key-ring generation is 14, while read-back says 12.
                    generation: 12,
                    origin: "https://fastmcp.invalid",
                }),
                store.clone(),
            )
            .expect("install verifier and durable activation store");
        provider
            .begin_id_token_signing_activation(signer, "https://fastmcp.invalid/oidc/jwks")
            .expect("begin pending activation");
        provider
            .publish_id_token_signing_jwks(canonical)
            .expect("publish exact public JWKS");

        let cx = Cx::for_testing();
        assert!(matches!(
            fastmcp_core::block_on(provider.activate_id_token_signing(&cx, signing_deadline())),
            Err(OidcError::SigningError(_))
        ));
        assert!(provider.activated_jwks_document().is_err());
        assert!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("durable store remains readable")
                .is_none()
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn external_read_back_receipt_and_canary_verification_activate_exact_signer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&calls),
        }));
        let canonical = signer
            .canonical_public_jwks()
            .expect("canonical public JWKS");
        let provider = provider_with_published_jwks(
            Arc::new(OAuthServer::with_defaults()),
            Arc::clone(&signer),
        );

        assert_eq!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .expect("bound public JWKS read-back"),
            canonical.as_bytes(),
        );
        let cx = Cx::for_testing();
        assert!(
            fastmcp_core::block_on(provider.activate_id_token_signing(&cx, signing_deadline()))
                .is_ok()
        );
        assert_eq!(
            provider.activated_jwks_document().expect("active JWKS"),
            canonical.as_bytes()
        );
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn durable_revocation_fences_id_token_before_external_signing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&calls),
        }));
        let canonical = signer
            .canonical_public_jwks()
            .expect("canonical public JWKS");
        let store = Arc::new(TestDurableActivationStore::default());
        let (oauth, issued) = issue_access_token(&["openid"]);
        let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("OIDC provider");
        provider
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: canonical.as_bytes().to_vec(),
                    generation: signer.binding().ring_generation(),
                    origin: "https://fastmcp.invalid",
                }),
                store.clone(),
            )
            .expect("install activation dependencies");
        provider
            .begin_id_token_signing_activation(signer, "https://fastmcp.invalid/oidc/jwks")
            .expect("begin pending activation");
        provider
            .publish_id_token_signing_jwks(canonical)
            .expect("publish canonical JWKS");
        activate_provider(&provider).expect("activate signer before durable revocation");
        let prior = store
            .load(&Cx::for_testing(), "https://fastmcp.invalid/")
            .expect("load active durable record")
            .expect("active durable record");
        let revoked = OidcSigningActivationStoreRecord::new(
            prior.issuer().to_string(),
            prior.key_ring_generation(),
            prior
                .activation_generation()
                .checked_add(1)
                .expect("bounded test generation"),
            OidcSigningActivationStatus::Revoked,
            prior.maximum_id_token_expires_at(),
            prior.key_id_maximum_id_token_expires_at().clone(),
        )
        .expect("bounded revoked durable record");
        *store.record.lock().expect("test durable store lock") = Some(revoked);

        let result = fastmcp_core::block_on(provider.issue_id_token(
            &Cx::for_testing(),
            &issued.access_token,
            None,
            signing_deadline(),
        ));
        assert!(matches!(result, Err(OidcError::SigningError(_))));
        assert_eq!(
            calls.load(Ordering::Acquire),
            1,
            "only activation canary dispatched"
        );
    }

    #[test]
    fn activated_external_custody_backend_issues_dynamic_verified_id_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let signer =
            dynamic_test_signer(Arc::clone(&backend) as Arc<dyn ExternalRs256SignerBackend>);
        let retained = test_signer(Arc::new(UnexpectedBackend {
            calls: Arc::new(AtomicUsize::new(0)),
        }));
        let key_ring = Rs256PublicKeyRing::new(signer, vec![retained], 24)
            .expect("dynamic active signer retains one published verification key");
        let (oauth, issued) = issue_access_token(&["openid"]);
        let expected_subject = oauth
            .validate_access_token(&issued.access_token)
            .expect("test access token remains open")
            .subject
            .expect("OIDC access token carries a subject");
        let provider = provider_with_published_key_ring_observed_generation(
            Arc::clone(&oauth),
            key_ring,
            None,
        );
        activate_provider(&provider).expect("dynamic external signer activates through read-back");

        let token = fastmcp_core::block_on(provider.issue_id_token(
            &Cx::for_testing(),
            &issued.access_token,
            Some("dynamic-nonce"),
            signing_deadline(),
        ))
        .expect("active externally-custodied signer issues a compact ID token");
        let public_jwks = provider
            .activated_jwks_document()
            .expect("published overlapping JWKS remains active");
        let read_back_keys =
            AdmittedRsaJwks::from_json(&public_jwks).expect("published JWKS admits");
        assert_eq!(
            read_back_keys.len(),
            2,
            "active plus retained public key overlap"
        );
        let verified = verify_compact_jws_rs256(&token.raw, &read_back_keys)
            .expect("dynamic compact ID token verifies against published JWKS");
        let payload = verified.claims();

        assert_eq!(token.claims.iss, provider.config().issuer);
        assert_eq!(token.claims.sub, expected_subject);
        assert_eq!(token.claims.aud, TEST_CLIENT_ID);
        assert_eq!(
            payload.get("iss"),
            Some(&serde_json::json!(token.claims.iss))
        );
        assert_eq!(
            payload.get("sub"),
            Some(&serde_json::json!(token.claims.sub))
        );
        assert_eq!(payload.get("aud"), Some(&serde_json::json!(TEST_CLIENT_ID)));
        assert_eq!(
            payload.get("iat"),
            Some(&serde_json::json!(token.claims.iat))
        );
        assert_eq!(
            payload.get("exp"),
            Some(&serde_json::json!(token.claims.exp))
        );
        assert_eq!(
            payload.get("nonce"),
            Some(&serde_json::json!("dynamic-nonce"))
        );
        assert!(token.claims.exp > token.claims.iat);

        let signing_input = token
            .raw
            .rsplit_once('.')
            .expect("compact ID token has a signature")
            .0;
        let captured_inputs = backend
            .signing_inputs
            .lock()
            .expect("dynamic signer input capture lock");
        assert_eq!(
            captured_inputs.len(),
            2,
            "one canary plus one ID-token operation"
        );
        assert_eq!(captured_inputs[1], signing_input.as_bytes());
        assert_ne!(captured_inputs[0], captured_inputs[1]);
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn active_generation_rotates_through_published_successor_with_public_overlap() {
        let active_calls = Arc::new(AtomicUsize::new(0));
        let active = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&active_calls),
        }));
        let initial_jwks = active
            .canonical_public_jwks()
            .expect("initial canonical JWKS");
        let verifier = Arc::new(MutableReadBackVerifier {
            evidence: Mutex::new((
                initial_jwks.as_bytes().to_vec(),
                active.binding().ring_generation(),
                "https://fastmcp.invalid",
            )),
        });
        let provider = OidcProvider::with_defaults(Arc::new(OAuthServer::with_defaults()))
            .expect("OIDC provider");
        provider
            .set_id_token_signing_activation_dependencies(
                verifier.clone(),
                Arc::new(TestDurableActivationStore::default()),
            )
            .expect("install mutable read-back verifier");
        provider
            .begin_id_token_signing_activation(
                Arc::clone(&active),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("begin initial Pending activation");
        provider
            .publish_id_token_signing_jwks(initial_jwks)
            .expect("publish initial public JWKS");
        activate_provider(&provider).expect("initial external signer activates");

        let successor_calls = Arc::new(AtomicUsize::new(0));
        let successor_backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&successor_calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let successor = dynamic_test_signer(
            Arc::clone(&successor_backend) as Arc<dyn ExternalRs256SignerBackend>
        );
        let successor_ring = Rs256PublicKeyRing::new(successor, vec![active], 24)
            .expect("successor ring retains active verification key");
        let successor_jwks = successor_ring
            .canonical_public_jwks()
            .expect("successor canonical overlapping JWKS");
        provider
            .begin_id_token_signing_key_ring_rotation(
                successor_ring,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            )
            .expect("Active generation accepts a successor Pending transition");
        verifier.set(
            successor_jwks.as_bytes().to_vec(),
            successor_jwks.generation(),
        );
        provider
            .publish_id_token_signing_key_ring_jwks(successor_jwks)
            .expect("successor reaches Published with public overlap");
        assert_eq!(
            AdmittedRsaJwks::from_json(
                &provider
                    .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                    .expect("published successor JWKS"),
            )
            .expect("published successor JWKS admits")
            .len(),
            2,
        );
        activate_provider(&provider).expect("successor becomes Active after external read-back");
        assert_eq!(
            AdmittedRsaJwks::from_json(
                &provider
                    .activated_jwks_document()
                    .expect("active successor JWKS"),
            )
            .expect("active successor JWKS admits")
            .len(),
            2,
        );
        assert_eq!(active_calls.load(Ordering::Acquire), 1);
        assert_eq!(successor_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn rh5_second_rotation_retains_older_live_key_and_carries_durable_expiry() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let initial_backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&initial_calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let initial = dynamic_test_signer_with_kid(
            Arc::clone(&initial_backend) as Arc<dyn ExternalRs256SignerBackend>,
            "dynamic-old",
            Rs256SigningBinding::new(21, 22, 23, 24).expect("initial dynamic signer generations"),
        );
        let initial_jwks = initial
            .canonical_public_jwks()
            .expect("initial dynamic JWKS");
        let verifier = Arc::new(MutableReadBackVerifier {
            evidence: Mutex::new((
                initial_jwks.as_bytes().to_vec(),
                initial.binding().ring_generation(),
                "https://fastmcp.invalid",
            )),
        });
        let store = Arc::new(TestDurableActivationStore::default());
        let (oauth, issued) = issue_access_token(&["openid"]);
        let provider = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("OIDC provider");
        provider
            .set_id_token_signing_activation_dependencies(verifier.clone(), store.clone())
            .expect("install mutable verifier and durable store");
        provider
            .begin_id_token_signing_activation(
                Arc::clone(&initial),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("begin initial activation");
        provider
            .publish_id_token_signing_jwks(initial_jwks)
            .expect("publish initial dynamic JWKS");
        activate_provider(&provider).expect("activate initial dynamic signer");
        let issued_token = fastmcp_core::block_on(provider.issue_id_token(
            &Cx::for_testing(),
            &issued.access_token,
            Some("rotation-live-key"),
            signing_deadline(),
        ))
        .expect("issue token under the oldest key");

        let middle_calls = Arc::new(AtomicUsize::new(0));
        let middle = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&middle_calls),
        }));
        let first_successor =
            Rs256PublicKeyRing::new(Arc::clone(&middle), vec![Arc::clone(&initial)], 25)
                .expect("first successor retains the issued-token key");
        let first_successor_jwks = first_successor
            .canonical_public_jwks()
            .expect("first successor JWKS");
        provider
            .begin_id_token_signing_key_ring_rotation(
                first_successor,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            )
            .expect("first successor begins from Active");
        verifier.set(
            first_successor_jwks.as_bytes().to_vec(),
            first_successor_jwks.generation(),
        );
        provider
            .publish_id_token_signing_key_ring_jwks(first_successor_jwks)
            .expect("first successor publishes overlap");
        activate_provider(&provider).expect("first successor activates");

        let final_calls = Arc::new(AtomicUsize::new(0));
        let final_backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&final_calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let final_signer = dynamic_test_signer_with_kid(
            Arc::clone(&final_backend) as Arc<dyn ExternalRs256SignerBackend>,
            "dynamic-successor",
            Rs256SigningBinding::new(31, 32, 33, 34).expect("final dynamic signer generations"),
        );
        let drops_oldest =
            Rs256PublicKeyRing::new(Arc::clone(&final_signer), vec![Arc::clone(&middle)], 26)
                .expect("well-formed but incomplete second successor");
        assert!(matches!(
            provider.begin_id_token_signing_key_ring_rotation(
                drops_oldest,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            ),
            Err(OidcError::SigningError(_))
        ));
        assert_eq!(final_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            AdmittedRsaJwks::from_json(
                &provider
                    .activated_jwks_document()
                    .expect("rejected successor leaves active JWKS unchanged"),
            )
            .expect("unchanged active JWKS admits")
            .len(),
            2,
        );
        assert!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("read unchanged durable record")
                .expect("active durable record remains present")
                .maximum_id_token_expires_at()
                >= issued_token.claims.exp
        );

        let final_successor = Rs256PublicKeyRing::new(
            final_signer,
            vec![Arc::clone(&middle), Arc::clone(&initial)],
            26,
        )
        .expect("second successor retains every still-live key");
        let final_jwks = final_successor
            .canonical_public_jwks()
            .expect("second successor JWKS");
        provider
            .begin_id_token_signing_key_ring_rotation(
                final_successor,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            )
            .expect("second successor admits only with the oldest key retained");
        verifier.set(final_jwks.as_bytes().to_vec(), final_jwks.generation());
        provider
            .publish_id_token_signing_key_ring_jwks(final_jwks)
            .expect("second successor publishes all live keys");
        activate_provider(&provider).expect("second successor activates");

        assert_eq!(
            AdmittedRsaJwks::from_json(
                &provider
                    .activated_jwks_document()
                    .expect("second successor active JWKS"),
            )
            .expect("second successor JWKS admits")
            .len(),
            3,
        );
        assert!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("read durable record")
                .expect("durable active record")
                .maximum_id_token_expires_at()
                >= issued_token.claims.exp
        );
        assert_eq!(initial_calls.load(Ordering::Acquire), 2);
        assert_eq!(middle_calls.load(Ordering::Acquire), 1);
        assert_eq!(final_calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn rh5_restart_rebuilds_durable_live_key_expiry_before_rotation() {
        let initial_calls = Arc::new(AtomicUsize::new(0));
        let initial_backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&initial_calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let initial = dynamic_test_signer_with_kid(
            Arc::clone(&initial_backend) as Arc<dyn ExternalRs256SignerBackend>,
            "restart-live-old",
            Rs256SigningBinding::new(41, 42, 43, 44).expect("initial generations"),
        );
        let initial_jwks = initial
            .canonical_public_jwks()
            .expect("initial canonical JWKS");
        let verifier = Arc::new(MutableReadBackVerifier {
            evidence: Mutex::new((
                initial_jwks.as_bytes().to_vec(),
                initial.binding().ring_generation(),
                "https://fastmcp.invalid",
            )),
        });
        let store = Arc::new(TestDurableActivationStore::default());
        let (oauth, issued) = issue_access_token(&["openid"]);
        let first = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("first provider");
        first
            .set_id_token_signing_activation_dependencies(verifier.clone(), store.clone())
            .expect("first activation dependencies");
        first
            .begin_id_token_signing_activation(
                Arc::clone(&initial),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("begin first activation");
        first
            .publish_id_token_signing_jwks(initial_jwks)
            .expect("publish first JWKS");
        activate_provider(&first).expect("activate first signer");
        let token = fastmcp_core::block_on(first.issue_id_token(
            &Cx::for_testing(),
            &issued.access_token,
            Some("restart-live-key"),
            signing_deadline(),
        ))
        .expect("old key signs an ID token before restart");
        let before_restart = store
            .load(&Cx::for_testing(), "https://fastmcp.invalid/")
            .expect("load first durable record")
            .expect("first durable record");
        assert_eq!(
            before_restart
                .key_id_maximum_id_token_expires_at()
                .get("restart-live-old")
                .map(OidcSigningKeyExpiry::expires_at),
            Some(token.claims.exp),
        );

        let restarted = OidcProvider::with_defaults(oauth).expect("restarted provider");
        restarted
            .set_id_token_signing_activation_dependencies(verifier.clone(), store.clone())
            .expect("restart activation dependencies");
        restarted
            .begin_id_token_signing_activation(
                Arc::clone(&initial),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("restart begins Pending rather than restoring memory");
        restarted
            .publish_id_token_signing_jwks(
                initial
                    .canonical_public_jwks()
                    .expect("restart canonical JWKS"),
            )
            .expect("restart republishes exact JWKS");
        activate_provider(&restarted).expect("restart read-back rebuilds durable live keys");
        let after_restart = store
            .load(&Cx::for_testing(), "https://fastmcp.invalid/")
            .expect("load restart durable record")
            .expect("restart durable record");
        assert_eq!(
            after_restart
                .key_id_maximum_id_token_expires_at()
                .get("restart-live-old")
                .map(OidcSigningKeyExpiry::expires_at),
            Some(token.claims.exp),
        );

        let successor_calls = Arc::new(AtomicUsize::new(0));
        let successor = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&successor_calls),
        }));
        let drops_live_old = Rs256PublicKeyRing::new(Arc::clone(&successor), Vec::new(), 45)
            .expect("well-formed restart successor with the old live key omitted");
        assert!(matches!(
            restarted.begin_id_token_signing_key_ring_rotation(
                drops_live_old,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            ),
            Err(OidcError::SigningError(_))
        ));
        assert_eq!(successor_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("load durable state after omitted-key rotation"),
            Some(after_restart.clone()),
        );
        let substituted_live_old = test_signer_with_kid_and_binding(
            Arc::new(FixedCanaryBackend {
                calls: Arc::clone(&successor_calls),
            }),
            "restart-live-old",
            Rs256SigningBinding::new(51, 52, 53, 54).expect("same-kid substitute generations"),
        );
        assert_eq!(substituted_live_old.key_id(), initial.key_id());
        assert_ne!(
            substituted_live_old
                .canonical_public_jwks()
                .expect("same-kid substitute identity")
                .as_bytes(),
            initial
                .canonical_public_jwks()
                .expect("durable old-key identity")
                .as_bytes(),
        );
        let substitutes_live_old =
            Rs256PublicKeyRing::new(Arc::clone(&successor), vec![substituted_live_old], 45)
                .expect("well-formed successor that changes only old RSA material");
        assert!(matches!(
            restarted.begin_id_token_signing_key_ring_rotation(
                substitutes_live_old,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            ),
            Err(OidcError::SigningError(_))
        ));
        assert_eq!(successor_calls.load(Ordering::Acquire), 0);
        assert_eq!(
            AdmittedRsaJwks::from_json(
                &restarted
                    .activated_jwks_document()
                    .expect("failed restart rotation leaves active JWKS unchanged"),
            )
            .expect("unchanged restart JWKS admits")
            .len(),
            1,
        );
        assert_eq!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("load durable state after rejected rotation"),
            Some(after_restart.clone()),
        );

        let retains_live_old = Rs256PublicKeyRing::new(successor, vec![initial], 45)
            .expect("successor retains restart-live verification key");
        let successor_jwks = retains_live_old
            .canonical_public_jwks()
            .expect("successor overlap JWKS");
        restarted
            .begin_id_token_signing_key_ring_rotation(
                retains_live_old,
                vec!["https://fastmcp.invalid/oidc/jwks".to_string()],
            )
            .expect("durably retained live key admits successor rotation");
        verifier.set(
            successor_jwks.as_bytes().to_vec(),
            successor_jwks.generation(),
        );
        restarted
            .publish_id_token_signing_key_ring_jwks(successor_jwks)
            .expect("publish restart successor overlap");
        activate_provider(&restarted).expect("activate restart successor with retained old key");
        let after_successor = store
            .load(&Cx::for_testing(), "https://fastmcp.invalid/")
            .expect("load successor durable record")
            .expect("successor durable record");
        assert_eq!(
            after_successor
                .key_id_maximum_id_token_expires_at()
                .get("restart-live-old")
                .map(OidcSigningKeyExpiry::expires_at),
            Some(token.claims.exp),
        );
        assert_eq!(
            AdmittedRsaJwks::from_json(
                &restarted
                    .activated_jwks_document()
                    .expect("restart successor active JWKS"),
            )
            .expect("restart successor JWKS admits")
            .len(),
            2,
        );
    }

    #[test]
    fn cancellation_after_canary_dispatch_exposes_no_active_signer() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(CancellationAfterDispatchBackend {
            calls: Arc::clone(&calls),
        }));
        let provider = provider_with_published_jwks(Arc::new(OAuthServer::with_defaults()), signer);
        assert!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .is_ok()
        );
        let cx = Cx::for_testing();

        let result =
            fastmcp_core::block_on(provider.activate_id_token_signing(&cx, signing_deadline()));
        assert!(matches!(
            result,
            Err(OidcError::ExternalSigning(
                JwsSigningError::CancelledAfterDispatch(_)
            ))
        ));
        assert!(cx.is_cancel_requested());
        assert!(provider.activated_jwks_document().is_err());
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }

    #[test]
    fn cancellation_after_id_token_signing_exposes_no_token() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(CancelIdTokenAfterSigningBackend {
            calls: Arc::clone(&calls),
        }));
        let (oauth, issued) = issue_access_token(&["openid"]);
        let provider = provider_with_published_jwks(oauth, signer);
        activate_provider(&provider).expect("canary activation remains valid");
        let cx = Cx::for_testing();
        let result = fastmcp_core::block_on(provider.issue_id_token(
            &cx,
            &issued.access_token,
            None,
            signing_deadline(),
        ));
        assert!(matches!(
            result,
            Err(OidcError::ExternalSigning(
                JwsSigningError::CancelledAfterDispatch(_)
            ))
        ));
        assert!(cx.is_cancel_requested());
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn retirement_refuses_live_tokens_then_closes_issuance_while_retaining_public_jwks() {
        let calls = Arc::new(AtomicUsize::new(0));
        let backend = Arc::new(TestOnlyDynamicRs256Backend {
            calls: Arc::clone(&calls),
            signing_inputs: Mutex::new(Vec::new()),
        });
        let signer =
            dynamic_test_signer(Arc::clone(&backend) as Arc<dyn ExternalRs256SignerBackend>);
        let (oauth, issued) = issue_access_token(&["openid"]);
        let provider = provider_with_published_jwks(oauth, signer);
        activate_provider(&provider).expect("activate external signer generation");
        let token = fastmcp_core::block_on(provider.issue_id_token(
            &Cx::for_testing(),
            &issued.access_token,
            None,
            signing_deadline(),
        ))
        .expect("issue ID token to establish durable retirement fence");
        assert!(
            provider
                .retire_id_token_signing_generation(&Cx::for_testing(), token.claims.exp - 1)
                .is_err()
        );
        provider
            .retire_id_token_signing_generation(&Cx::for_testing(), token.claims.exp)
            .expect("retire only after maximum token expiry");
        assert!(provider.activated_jwks_document().is_err());
        assert!(
            provider
                .published_jwks_document("https://fastmcp.invalid/oidc/jwks")
                .is_ok()
        );
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }

    #[test]
    fn durable_store_fences_restart_reactivation_without_restoring_active_memory() {
        let calls = Arc::new(AtomicUsize::new(0));
        let signer = test_signer(Arc::new(FixedCanaryBackend {
            calls: Arc::clone(&calls),
        }));
        let jwks = signer
            .canonical_public_jwks()
            .expect("canonical public JWKS")
            .as_bytes()
            .to_vec();
        let store = Arc::new(TestDurableActivationStore::default());
        let oauth = Arc::new(OAuthServer::with_defaults());

        let first = OidcProvider::with_defaults(Arc::clone(&oauth)).expect("first provider");
        first
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: jwks.clone(),
                    generation: signer.binding().ring_generation(),
                    origin: "https://fastmcp.invalid",
                }),
                store.clone(),
            )
            .expect("first dependencies");
        first
            .begin_id_token_signing_activation(
                Arc::clone(&signer),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("first pending");
        first
            .publish_id_token_signing_jwks(
                signer
                    .canonical_public_jwks()
                    .expect("first canonical JWKS"),
            )
            .expect("first publication");
        fastmcp_core::block_on(
            first.activate_id_token_signing(&Cx::for_testing(), signing_deadline()),
        )
        .expect("first activation");

        let restarted = OidcProvider::with_defaults(oauth).expect("fresh-process provider");
        assert!(restarted.activated_jwks_document().is_err());
        restarted
            .set_id_token_signing_activation_dependencies(
                Arc::new(FixedReadBackVerifier {
                    bytes: jwks,
                    generation: signer.binding().ring_generation(),
                    origin: "https://fastmcp.invalid",
                }),
                store.clone(),
            )
            .expect("restart dependencies");
        restarted
            .begin_id_token_signing_activation(
                Arc::clone(&signer),
                "https://fastmcp.invalid/oidc/jwks",
            )
            .expect("restart pending");
        restarted
            .publish_id_token_signing_jwks(
                signer
                    .canonical_public_jwks()
                    .expect("restart canonical JWKS"),
            )
            .expect("restart publication");
        fastmcp_core::block_on(
            restarted.activate_id_token_signing(&Cx::for_testing(), signing_deadline()),
        )
        .expect("fresh read-back and CAS are required after restart");
        assert_eq!(
            store
                .load(&Cx::for_testing(), "https://fastmcp.invalid/")
                .expect("durable store read")
                .expect("durable activation record")
                .activation_generation(),
            2,
        );
        assert_eq!(calls.load(Ordering::Acquire), 2);
    }
}
