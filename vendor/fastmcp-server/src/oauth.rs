//! OAuth 2.0/2.1 authorization-server implementation code for MCP.
//!
//! This module contains authorization-server building blocks for MCP servers:
//!
//! - **Authorization Code Flow** with PKCE (required for OAuth 2.1)
//! - **Token Issuance** - Access tokens and refresh tokens
//! - **Token Revocation** - RFC 7009 token revocation
//! - **Client Registration** - Dynamic client registration
//! - **Scope Validation** - Fine-grained scope control
//! - **Redirect URI Validation** - Security-critical validation
//!
//! # Architecture
//!
//! The OAuth server is designed to be modular:
//!
//! - [`OAuthServer`]: Main authorization server component
//! - [`OAuthClient`]: Registered OAuth client
//! - [`OAuthClientMetadata`]: Secret-free registered-client metadata
//! - [`AuthorizationCode`]: Temporary code for token exchange
//! - [`OAuthToken`]: Access and refresh tokens
//! - [`OAuthTokenVerifier`]: Implements [`TokenVerifier`] for MCP integration
//!
//! # Security posture
//!
//! These are implementation policies. AUTH promotion and MCP 2026-07-28
//! conformance remain unverified:
//!
//! - S256 PKCE is required by the implemented authorization-code path
//! - Redirect URIs reject userinfo/fragments and otherwise require an exact
//!   match, except that loopback ports may vary
//! - Token material is drawn through the core security-identifier API
//! - Authorization codes are single-use and expire quickly
//! - Refresh tokens rotate on successful use; retained replay markers revoke
//!   the complete live grant family before replay is rejected
//! - Configurable retained-state counts have per-field and aggregate hard
//!   ceilings. These bound entry counts, not exact heap bytes, and do not
//!   qualify this implementation for production OAuth use
//!
//! # Example
//!
//! ```ignore
//! use std::sync::Arc;
//! use fastmcp_rust::oauth::{OAuthClient, OAuthServer, OAuthServerConfig};
//! use fastmcp_rust::{modern::ServerBuilder, TokenAuthProvider};
//! use fastmcp_core::{CanonicalHttpUrl, CanonicalResourceId, CanonicalResourceIdPolicy};
//!
//! let oauth = Arc::new(OAuthServer::new(OAuthServerConfig::default()));
//!
//! // Register a client
//! let client = OAuthClient::builder("my-client")
//!     .redirect_uri("http://127.0.0.1:3000/callback")
//!     .scope("read")
//!     .scope("write")
//!     .build()?;
//!
//! oauth.register_client(client)?;
//!
//! // Inside the application's async entry point, with its supplied `cx`.
//! let endpoint = CanonicalHttpUrl::parse("https://mcp.example/mcp")?;
//! let resource = CanonicalResourceId::parse_for_endpoint(
//!     endpoint.as_str(), &endpoint, CanonicalResourceIdPolicy::DEFAULT,
//! )?;
//! let verifier = oauth.token_verifier(resource);
//! ServerBuilder::new("my-server", "1.0.0")
//!     .auth_provider(TokenAuthProvider::new(verifier))
//!     .build()
//!     .run_http_with_cx(cx, "127.0.0.1:8080")
//!     .await;
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use fastmcp_core::{
    AccessToken, AuthContext, CanonicalResourceId, McpContext, McpError, McpErrorCode, McpResult,
    SecurityIdentifier, Sha256Digest, draw_security_identifier, sha256_bounded,
};
use url::{Host, Url};
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

use crate::auth::{AuthRequest, TokenVerifier};
#[cfg(feature = "builtin-auth-server")]
use crate::oidc::OidcProvider;

const PKCE_CODE_VERIFIER_MIN_BYTES: usize = 43;
const PKCE_CODE_VERIFIER_MAX_BYTES: usize = 128;
const OAUTH_OPAQUE_CREDENTIAL_BYTES: usize = 43;
const CLIENT_SECRET_VERIFIER_DOMAIN: &[u8] = b"fastmcp:oauth:client-secret:v1\0";
const AUTHORIZATION_CODE_DIGEST_DOMAIN: &[u8] = b"fastmcp:oauth:authorization-code:v1\0";
const ACCESS_TOKEN_DIGEST_DOMAIN: &[u8] = b"fastmcp:oauth:access-token:v1\0";
const REFRESH_TOKEN_DIGEST_DOMAIN: &[u8] = b"fastmcp:oauth:refresh-token:v1\0";
const AUTHORIZATION_GRANT_ID_DOMAIN: &[u8] = b"fastmcp:oauth:authorization-grant-id:v1\0";
const DIRECT_GRANT_ID_DOMAIN: &[u8] = b"fastmcp:oauth:direct-grant-id:v1\0";
const OAUTH_SESSION_OWNER_DOMAIN: &[u8] = b"fastmcp:oauth:session-owner:v1\0";
const OAUTH_REGISTRATION_EPOCH_BYTES: usize = 32;
const CLIENT_SECRET_SALT_BYTES: usize = 32;
const MAX_CLIENT_SECRET_VERIFIER_INPUT_BYTES: usize = CLIENT_SECRET_VERIFIER_DOMAIN.len()
    + CLIENT_SECRET_SALT_BYTES
    + MAX_OAUTH_CLIENT_CREDENTIAL_BYTES;
const MAX_OPAQUE_CREDENTIAL_DIGEST_INPUT_BYTES: usize =
    AUTHORIZATION_CODE_DIGEST_DOMAIN.len() + OAUTH_OPAQUE_CREDENTIAL_BYTES;
const MAX_GRANT_ID_DERIVATION_INPUT_BYTES: usize = AUTHORIZATION_GRANT_ID_DOMAIN.len() + 64;
const DUMMY_CLIENT_SECRET_SALT: [u8; CLIENT_SECRET_SALT_BYTES] = [0x5a; CLIENT_SECRET_SALT_BYTES];
const DUMMY_CLIENT_SECRET_DIGEST: Sha256Digest = Sha256Digest::from_bytes([0xa5; 32]);

/// Maximum UTF-8 byte length of the configured OAuth issuer URL.
pub const MAX_OAUTH_ISSUER_BYTES: usize = 2_048;

/// Maximum UTF-8 byte length of a retained OAuth client identifier.
pub const MAX_OAUTH_CLIENT_ID_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a retained OAuth client credential.
pub const MAX_OAUTH_CLIENT_CREDENTIAL_BYTES: usize = 1_024;
/// Maximum number of redirect URIs retained for one OAuth client.
pub const MAX_OAUTH_REDIRECT_URIS_PER_CLIENT: usize = 16;
/// Maximum UTF-8 byte length of one retained OAuth redirect URI.
pub const MAX_OAUTH_REDIRECT_URI_BYTES: usize = 2_048;
/// Maximum number of scopes retained for one OAuth client.
pub const MAX_OAUTH_SCOPES_PER_CLIENT: usize = 64;
/// Maximum UTF-8 byte length of one retained OAuth scope.
pub const MAX_OAUTH_SCOPE_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a retained OAuth client display name.
pub const MAX_OAUTH_CLIENT_NAME_BYTES: usize = 256;
/// Maximum UTF-8 byte length of a retained OAuth client description.
pub const MAX_OAUTH_CLIENT_DESCRIPTION_BYTES: usize = 4_096;
/// Maximum UTF-8 byte length of an authorization grant subject.
pub const MAX_OAUTH_SUBJECT_BYTES: usize = 1_024;
/// Maximum UTF-8 byte length of an RFC 8707 authorization resource indicator.
pub const MAX_OAUTH_RESOURCE_BYTES: usize = 2_048;
const MAX_OAUTH_SESSION_OWNER_INPUT_BYTES: usize = OAUTH_SESSION_OWNER_DOMAIN.len()
    + 8
    + MAX_OAUTH_ISSUER_BYTES
    + 8
    + MAX_OAUTH_CLIENT_ID_BYTES
    + OAUTH_REGISTRATION_EPOCH_BYTES
    + 1
    + 8
    + MAX_OAUTH_SUBJECT_BYTES
    + 1
    + 8
    + MAX_OAUTH_RESOURCE_BYTES;
/// Maximum UTF-8 byte length of an OAuth authorization `state` value.
pub const MAX_OAUTH_STATE_BYTES: usize = 4_096;

/// Maximum encoded bytes admitted by one authorization query.
///
/// This bound applies before percent decoding, so an attacker cannot make a
/// small wire request allocate an unbounded decoded value.
pub const MAX_OAUTH_AUTHORIZATION_QUERY_BYTES: usize = 16 * 1_024;
/// Maximum encoded bytes admitted by one token-like form body.
pub const MAX_OAUTH_FORM_BODY_BYTES: usize = 16 * 1_024;
/// Maximum key/value pairs admitted by one OAuth endpoint request.
pub const MAX_OAUTH_PARAMETER_PAIRS: usize = 64;
/// Maximum decoded bytes in one OAuth parameter name.
pub const MAX_OAUTH_PARAMETER_NAME_BYTES: usize = 256;
/// Maximum decoded bytes in one OAuth parameter value.
pub const MAX_OAUTH_PARAMETER_VALUE_BYTES: usize = MAX_OAUTH_STATE_BYTES;

const OAUTH_ISSUER_ERROR: &str = "OAuth issuer URL is invalid or outside retained-value bounds";
const OAUTH_CLIENT_ID_RETENTION_ERROR: &str = "OAuth client_id is outside retained-value bounds";
const OAUTH_CLIENT_CREDENTIAL_RETENTION_ERROR: &str =
    "OAuth client credential is outside retained-value bounds";
const OAUTH_CLIENT_CREDENTIAL_CLASS_ERROR: &str =
    "OAuth client credential classification is inconsistent";
const OAUTH_CLIENT_REDIRECT_REQUIRED_ERROR: &str =
    "OAuth client requires at least one redirect URI";
const OAUTH_CLIENT_REDIRECT_COUNT_ERROR: &str =
    "OAuth client redirect URI count exceeds retention bounds";
const OAUTH_CLIENT_REDIRECT_VALUE_ERROR: &str =
    "OAuth client redirect URI is outside retained-value bounds";
const OAUTH_CLIENT_SCOPE_COUNT_ERROR: &str = "OAuth client scope count exceeds retention bounds";
const OAUTH_CLIENT_SCOPE_VALUE_ERROR: &str = "OAuth client scope is outside retained-value bounds";
const OAUTH_CLIENT_NAME_RETENTION_ERROR: &str =
    "OAuth client name is invalid or outside retention bounds";
const OAUTH_CLIENT_DESCRIPTION_RETENTION_ERROR: &str =
    "OAuth client description is invalid or outside retention bounds";
const OAUTH_AUTHORIZATION_SUBJECT_RETENTION_ERROR: &str =
    "OAuth authorization subject is invalid or outside retention bounds";
const OAUTH_AUTHORIZATION_STATE_RETENTION_ERROR: &str =
    "OAuth authorization state is invalid or outside retention bounds";
const OAUTH_AUTHORIZATION_RESOURCE_RETENTION_ERROR: &str =
    "OAuth authorization resource is invalid or outside retention bounds";
const OAUTH_REQUEST_SCOPE_COUNT_ERROR: &str = "OAuth request scope count exceeds retention bounds";
const OAUTH_REQUEST_SCOPE_VALUE_ERROR: &str = "OAuth request scope is invalid or outside bounds";
const OAUTH_CLIENT_NOT_FOUND_ERROR: &str = "OAuth client not found";
const OAUTH_CLIENT_AUTHENTICATION_ERROR: &str = "client authentication failed";
const OAUTH_GRANT_TYPE_UNSUPPORTED_ERROR: &str = "OAuth grant_type is not supported";
const OAUTH_INVALID_GRANT_ERROR: &str = "OAuth grant is invalid";

// =============================================================================
// Raw OAuth parameter admission
// =============================================================================

/// The exact OAuth endpoint grammar used to admit an untrusted parameter
/// sequence.
///
/// Authorization parameters originate in a URI query; all other profiles
/// originate in an `application/x-www-form-urlencoded` request body. The
/// profile is selected by the HTTP adapter's route, not by a peer-controlled
/// content type or parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthParameterEndpoint {
    /// Authorization endpoint query parameters.
    AuthorizationQuery,
    /// Token endpoint form parameters.
    TokenForm,
    /// Token-revocation endpoint form parameters.
    RevocationForm,
    /// Token-introspection endpoint form parameters.
    IntrospectionForm,
}

impl OAuthParameterEndpoint {
    /// Returns the only permitted wire source for this endpoint profile.
    #[must_use]
    pub const fn source(self) -> OAuthParameterSource {
        match self {
            Self::AuthorizationQuery => OAuthParameterSource::Query,
            Self::TokenForm | Self::RevocationForm | Self::IntrospectionForm => {
                OAuthParameterSource::Form
            }
        }
    }

    const fn maximum_input_bytes(self) -> usize {
        match self {
            Self::AuthorizationQuery => MAX_OAUTH_AUTHORIZATION_QUERY_BYTES,
            Self::TokenForm | Self::RevocationForm | Self::IntrospectionForm => {
                MAX_OAUTH_FORM_BODY_BYTES
            }
        }
    }

    fn defined_parameter(self, name: &str) -> Option<OAuthParameterName> {
        match self {
            Self::AuthorizationQuery => match name {
                "response_type" => Some(OAuthParameterName::ResponseType),
                "client_id" => Some(OAuthParameterName::ClientId),
                "redirect_uri" => Some(OAuthParameterName::RedirectUri),
                "resource" => Some(OAuthParameterName::Resource),
                "scope" => Some(OAuthParameterName::Scope),
                "state" => Some(OAuthParameterName::State),
                "code_challenge" => Some(OAuthParameterName::CodeChallenge),
                "code_challenge_method" => Some(OAuthParameterName::CodeChallengeMethod),
                _ => None,
            },
            Self::TokenForm => match name {
                "grant_type" => Some(OAuthParameterName::GrantType),
                "code" => Some(OAuthParameterName::Code),
                "redirect_uri" => Some(OAuthParameterName::RedirectUri),
                "resource" => Some(OAuthParameterName::Resource),
                "client_id" => Some(OAuthParameterName::ClientId),
                "client_secret" => Some(OAuthParameterName::ClientSecret),
                "code_verifier" => Some(OAuthParameterName::CodeVerifier),
                "refresh_token" => Some(OAuthParameterName::RefreshToken),
                "scope" => Some(OAuthParameterName::Scope),
                _ => None,
            },
            Self::RevocationForm | Self::IntrospectionForm => match name {
                "token" => Some(OAuthParameterName::Token),
                "token_type_hint" => Some(OAuthParameterName::TokenTypeHint),
                "client_id" => Some(OAuthParameterName::ClientId),
                "client_secret" => Some(OAuthParameterName::ClientSecret),
                _ => None,
            },
        }
    }
}

/// The wire source that supplied an admitted parameter.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OAuthParameterSource {
    /// URI query text on the authorization endpoint.
    Query,
    /// `application/x-www-form-urlencoded` request body.
    Form,
}

/// A parameter name whose value may influence the matching endpoint.
///
/// Names outside the selected endpoint profile remain admitted as bounded,
/// ordered metadata in [`OAuthParameterAdmission::unknown_parameters`]. Their
/// values are discarded after validation and cannot influence endpoint parsing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OAuthParameterName {
    /// `response_type`
    ResponseType,
    /// `client_id`
    ClientId,
    /// `redirect_uri`
    RedirectUri,
    /// RFC 8707 `resource`
    Resource,
    /// `scope`
    Scope,
    /// `state`
    State,
    /// `code_challenge`
    CodeChallenge,
    /// `code_challenge_method`
    CodeChallengeMethod,
    /// `grant_type`
    GrantType,
    /// `code`
    Code,
    /// `client_secret`
    ClientSecret,
    /// `code_verifier`
    CodeVerifier,
    /// `refresh_token`
    RefreshToken,
    /// `token`
    Token,
    /// `token_type_hint`
    TokenTypeHint,
}

/// Immutable public native-HTTP routes for an [`OAuthServer`].
///
/// OAuth authorization, token, and revocation are always available. An OIDC
/// provider can add its fixed discovery and JWKS routes only after it has
/// bound an exact advertised public JWKS URI to an external signer.
#[derive(Clone)]
pub struct OAuthHttpRoutes {
    server: Arc<OAuthServer>,
    public_endpoint_base: String,
    /// The endpoint base's path with one trailing `/`; every fixed route is
    /// this prefix plus its name, matching `DiscoveryDocument`'s spelling.
    route_prefix: String,
    authorization_path: String,
    token_path: String,
    revocation_path: String,
    registration_path: Option<String>,
    #[cfg(feature = "builtin-auth-server")]
    oidc: Option<OidcHttpRoutes>,
}

/// Native public OIDC metadata routes bound to one OAuth server and issuer.
#[cfg(feature = "builtin-auth-server")]
#[derive(Clone)]
pub(crate) struct OidcHttpRoutes {
    provider: Arc<OidcProvider>,
    discovery_path: String,
    jwks_path: String,
    jwks_uri: String,
    userinfo_path: String,
}

#[cfg(feature = "builtin-auth-server")]
impl OidcHttpRoutes {
    pub(crate) fn provider(&self) -> &Arc<OidcProvider> {
        &self.provider
    }

    pub(crate) fn discovery_path(&self) -> &str {
        &self.discovery_path
    }

    pub(crate) fn jwks_path(&self) -> &str {
        &self.jwks_path
    }

    /// The route serving the discovery document's `userinfo_endpoint`.
    pub(crate) fn userinfo_path(&self) -> &str {
        &self.userinfo_path
    }

    pub(crate) fn jwks_uri(&self) -> &str {
        &self.jwks_uri
    }
}

impl std::fmt::Debug for OAuthHttpRoutes {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = formatter.debug_struct("OAuthHttpRoutes");
        debug
            .field("public_endpoint_base", &self.public_endpoint_base)
            .field("authorization_path", &self.authorization_path)
            .field("token_path", &self.token_path)
            .field("revocation_path", &self.revocation_path)
            .field("registration_path", &self.registration_path);
        #[cfg(feature = "builtin-auth-server")]
        debug
            .field(
                "oidc_discovery_path",
                &self.oidc.as_ref().map(|oidc| oidc.discovery_path.as_str()),
            )
            .field(
                "oidc_jwks_path",
                &self.oidc.as_ref().map(|oidc| oidc.jwks_path.as_str()),
            )
            .field(
                "oidc_userinfo_path",
                &self.oidc.as_ref().map(|oidc| oidc.userinfo_path.as_str()),
            );
        debug.finish_non_exhaustive()
    }
}

/// A public OAuth HTTP route configuration was unsafe or ambiguous.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthHttpRouteConfigurationError {
    /// The configured endpoint base was not a canonical HTTPS URL.
    InvalidPublicEndpointBase,
    /// The endpoint base did not share the configured OAuth issuer origin.
    IssuerOriginMismatch,
}

impl std::fmt::Display for OAuthHttpRouteConfigurationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidPublicEndpointBase => formatter.write_str(
                "OAuth public endpoint base must be a canonical HTTPS URL without query or fragment",
            ),
            Self::IssuerOriginMismatch => formatter.write_str(
                "OAuth public endpoint base must share the configured issuer origin",
            ),
        }
    }
}

impl std::error::Error for OAuthHttpRouteConfigurationError {}

impl OAuthHttpRoutes {
    /// Creates the fixed authorization, token, and revocation routes below an
    /// explicit public HTTPS endpoint base.
    ///
    /// For example, `https://auth.example.test/oauth` exposes
    /// `/oauth/authorize`, `/oauth/token`, and `/oauth/revoke`. The base is
    /// never inferred from a request Host or forwarded header.
    pub fn new(
        server: Arc<OAuthServer>,
        public_endpoint_base: impl Into<String>,
    ) -> Result<Self, OAuthHttpRouteConfigurationError> {
        let public_endpoint_base = public_endpoint_base.into();
        let Some(base) = parse_secure_endpoint(
            &public_endpoint_base,
            MAX_OAUTH_ISSUER_BYTES,
            EndpointSpelling::PathlessAuthority,
        ) else {
            return Err(OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase);
        };
        if base.scheme() != "https" || base.query().is_some() {
            return Err(OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase);
        }
        let Some(issuer) = parse_secure_endpoint(
            &server.config().issuer,
            MAX_OAUTH_ISSUER_BYTES,
            EndpointSpelling::PathlessAuthority,
        ) else {
            return Err(OAuthHttpRouteConfigurationError::IssuerOriginMismatch);
        };
        if base.scheme() != issuer.scheme()
            || base.host_str() != issuer.host_str()
            || base.port_or_known_default() != issuer.port_or_known_default()
        {
            return Err(OAuthHttpRouteConfigurationError::IssuerOriginMismatch);
        }

        let route_prefix = format!("{}/", base.path().trim_end_matches('/'));
        let route_path = |suffix: &str| format!("{route_prefix}{suffix}");
        let registration_path = server
            .config()
            .allow_public_clients
            .then(|| route_path("register"));
        let authorization_path = route_path("authorize");
        let token_path = route_path("token");
        let revocation_path = route_path("revoke");
        Ok(Self {
            server,
            public_endpoint_base,
            route_prefix,
            authorization_path,
            token_path,
            revocation_path,
            registration_path,
            #[cfg(feature = "builtin-auth-server")]
            oidc: None,
        })
    }

    /// Adds fixed OIDC discovery, JWKS and UserInfo routes for a provider that has
    /// already entered signer activation. The provider must be layered over
    /// this exact OAuth server; neither issuer nor endpoint paths are inferred
    /// from requests.
    #[cfg(feature = "builtin-auth-server")]
    pub fn with_oidc(
        mut self,
        provider: Arc<OidcProvider>,
    ) -> Result<Self, OAuthHttpRouteConfigurationError> {
        if !Arc::ptr_eq(provider.oauth(), &self.server) {
            return Err(OAuthHttpRouteConfigurationError::IssuerOriginMismatch);
        }
        let jwks_uri = provider
            .advertised_id_token_jwks_uri()
            .map_err(|_| OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase)?;
        let jwks = Url::parse(&jwks_uri)
            .map_err(|_| OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase)?;
        let issuer = Url::parse(&provider.config().issuer)
            .map_err(|_| OAuthHttpRouteConfigurationError::IssuerOriginMismatch)?;
        if jwks.origin() != issuer.origin() || jwks.path().is_empty() {
            return Err(OAuthHttpRouteConfigurationError::IssuerOriginMismatch);
        }
        let issuer_path = issuer.path().trim_matches('/');
        let discovery_path = if issuer_path.is_empty() {
            "/.well-known/openid-configuration".to_string()
        } else {
            format!("/.well-known/openid-configuration/{issuer_path}")
        };
        let candidate = OidcHttpRoutes {
            provider,
            discovery_path,
            jwks_path: jwks.path().to_string(),
            jwks_uri,
            userinfo_path: format!("{}userinfo", self.route_prefix),
        };
        let oauth_paths = [
            Some(self.authorization_path()),
            Some(self.token_path()),
            Some(self.revocation_path()),
            self.registration_path(),
        ];
        let oidc_paths = [
            candidate.discovery_path.as_str(),
            candidate.jwks_path.as_str(),
            candidate.userinfo_path.as_str(),
        ];
        if oidc_paths
            .iter()
            .any(|path| oauth_paths.contains(&Some(*path)))
            || oidc_paths
                .iter()
                .enumerate()
                .any(|(index, path)| oidc_paths[index + 1..].contains(path))
        {
            return Err(OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase);
        }
        self.oidc = Some(candidate);
        Ok(self)
    }

    /// Returns the configured public endpoint base.
    #[must_use]
    pub fn public_endpoint_base(&self) -> &str {
        &self.public_endpoint_base
    }

    /// Returns the exact authorization endpoint path.
    #[must_use]
    pub fn authorization_path(&self) -> &str {
        &self.authorization_path
    }

    /// Returns the exact token endpoint path.
    #[must_use]
    pub fn token_path(&self) -> &str {
        &self.token_path
    }

    /// Returns the exact revocation endpoint path.
    #[must_use]
    pub fn revocation_path(&self) -> &str {
        &self.revocation_path
    }

    /// Returns the native public-client registration route when enabled.
    #[must_use]
    pub fn registration_path(&self) -> Option<&str> {
        self.registration_path.as_deref()
    }

    pub(crate) fn server(&self) -> &Arc<OAuthServer> {
        &self.server
    }

    pub(crate) fn has_path(&self, path: &str) -> bool {
        path == self.authorization_path
            || path == self.token_path
            || path == self.revocation_path
            || self.registration_path.as_deref() == Some(path)
            || {
                #[cfg(feature = "builtin-auth-server")]
                {
                    self.oidc.as_ref().is_some_and(|oidc| {
                        path == oidc.discovery_path
                            || path == oidc.jwks_path
                            || path == oidc.userinfo_path
                    })
                }
                #[cfg(not(feature = "builtin-auth-server"))]
                {
                    false
                }
            }
    }

    #[cfg(feature = "builtin-auth-server")]
    pub(crate) fn oidc_routes(&self) -> Option<&OidcHttpRoutes> {
        self.oidc.as_ref()
    }

    pub(crate) fn validate_non_overlapping_paths<'a>(
        &self,
        occupied_paths: impl IntoIterator<Item = &'a str>,
    ) -> Result<(), OAuthHttpRouteConfigurationError> {
        if occupied_paths.into_iter().any(|occupied| {
            [
                Some(self.authorization_path()),
                Some(self.token_path()),
                Some(self.revocation_path()),
                self.registration_path(),
            ]
            .contains(&Some(occupied))
                || {
                    #[cfg(feature = "builtin-auth-server")]
                    {
                        self.oidc.as_ref().is_some_and(|oidc| {
                            occupied == oidc.discovery_path
                                || occupied == oidc.jwks_path
                                || occupied == oidc.userinfo_path
                        })
                    }
                    #[cfg(not(feature = "builtin-auth-server"))]
                    {
                        false
                    }
                }
        }) {
            return Err(OAuthHttpRouteConfigurationError::InvalidPublicEndpointBase);
        }
        Ok(())
    }
}

/// One decoded parameter retained in exact wire order.
#[derive(Debug, Clone)]
pub struct OAuthAdmittedParameter {
    source: OAuthParameterSource,
    ordinal: usize,
    name: String,
    value_len: usize,
    defined: bool,
}

impl OAuthAdmittedParameter {
    /// Returns the query or form source that supplied this value.
    #[must_use]
    pub const fn source(&self) -> OAuthParameterSource {
        self.source
    }

    /// Returns this parameter's zero-based wire order.
    #[must_use]
    pub const fn ordinal(&self) -> usize {
        self.ordinal
    }

    /// Returns the decoded parameter name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the decoded parameter value's byte length.
    ///
    /// The value itself is deliberately unavailable through the public
    /// diagnostics view. In particular, this prevents codes, refresh tokens,
    /// client secrets, and repeated unknown values from being copied into a
    /// log or inspection surface.
    #[must_use]
    pub const fn value_len(&self) -> usize {
        self.value_len
    }

    /// Returns whether the selected endpoint profile defines this name.
    #[must_use]
    pub const fn is_defined(&self) -> bool {
        self.defined
    }
}

/// A duplicate-aware, bounded admission result for one OAuth parameter source.
///
/// Empty defined values are retained in [`Self::parameters`] for diagnostics,
/// but are intentionally omitted from the sensitive taking surface. This gives the
/// endpoint parser one missing-value representation without allowing an empty
/// duplicate to evade defined-name duplicate rejection.
pub struct OAuthParameterAdmission {
    endpoint: OAuthParameterEndpoint,
    source: OAuthParameterSource,
    parameters: Vec<OAuthAdmittedParameter>,
    defined: HashMap<OAuthParameterName, OAuthSensitiveParameterValue>,
}

/// One defined OAuth value retained only for the next crate-local endpoint
/// parser.
///
/// This is intentionally neither `Clone` nor `Debug`. Values are moved out of
/// [`OAuthParameterAdmission`] with [`OAuthParameterAdmission::take_defined_value`]
/// and are zeroized if the endpoint declines to consume them.
pub(crate) struct OAuthSensitiveParameterValue {
    value: Zeroizing<String>,
}

impl OAuthSensitiveParameterValue {
    /// Moves the value into the endpoint's own typed request boundary.
    #[must_use]
    pub(crate) fn into_string(mut self) -> String {
        std::mem::take(&mut *self.value)
    }
}

impl std::fmt::Debug for OAuthParameterAdmission {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OAuthParameterAdmission")
            .field("endpoint", &self.endpoint)
            .field("source", &self.source)
            .field("parameter_count", &self.parameters.len())
            .field("defined_count", &self.defined.len())
            .finish()
    }
}

/// A rejection raised before endpoint authentication, grant handling, or
/// another stateful OAuth operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthParameterAdmissionError {
    /// The encoded query or form body exceeds its endpoint bound.
    InputTooLarge,
    /// The request carries more pairs than the shared endpoint bound permits.
    TooManyPairs,
    /// A decoded parameter name is empty.
    EmptyName,
    /// A decoded parameter name exceeds its bound.
    NameTooLarge,
    /// A decoded parameter value exceeds its bound.
    ValueTooLarge,
    /// Percent decoding was incomplete or contained a non-hex digit.
    MalformedPercentEncoding,
    /// A percent-decoded component was not valid UTF-8.
    InvalidUtf8,
    /// A decoded name or value contains a control character.
    ControlCharacter,
    /// A profile-defined name occurs more than once, including empty values.
    DuplicateDefinedParameter {
        /// The endpoint-defined name that was repeated.
        parameter: OAuthParameterName,
        /// The query or form source containing both occurrences.
        source: OAuthParameterSource,
        /// Wire order of the first occurrence.
        first_ordinal: usize,
        /// Wire order of the rejected duplicate occurrence.
        duplicate_ordinal: usize,
    },
}

impl std::fmt::Display for OAuthParameterAdmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self {
            Self::InputTooLarge => "OAuth parameter input exceeds the endpoint bound",
            Self::TooManyPairs => "OAuth parameter input contains too many pairs",
            Self::EmptyName => "OAuth parameter name is empty",
            Self::NameTooLarge => "OAuth parameter name exceeds the endpoint bound",
            Self::ValueTooLarge => "OAuth parameter value exceeds the endpoint bound",
            Self::MalformedPercentEncoding => "OAuth parameter percent encoding is malformed",
            Self::InvalidUtf8 => "OAuth parameter encoding is not valid UTF-8",
            Self::ControlCharacter => "OAuth parameter contains a control character",
            Self::DuplicateDefinedParameter { .. } => {
                "OAuth parameter input repeats a defined endpoint parameter"
            }
        };
        formatter.write_str(message)
    }
}

impl std::error::Error for OAuthParameterAdmissionError {}

impl OAuthParameterAdmission {
    /// Strictly admits one raw authorization query or token-like form body.
    ///
    /// This function is deliberately pure: it performs no redirect handling,
    /// authentication, token lookup, grant consumption, or other OAuth state
    /// mutation. This lower layer is not a production HTTP gate: a later
    /// adapter must select `endpoint` from its already-routed HTTP endpoint,
    /// enforce content type and transport policy, and perform endpoint auth,
    /// rate admission, and all stateful OAuth work after this parser returns.
    pub fn admit(
        endpoint: OAuthParameterEndpoint,
        input: &[u8],
    ) -> Result<Self, OAuthParameterAdmissionError> {
        if input.len() > endpoint.maximum_input_bytes() {
            return Err(OAuthParameterAdmissionError::InputTooLarge);
        }

        let source = endpoint.source();
        let mut parameters = Vec::new();
        let mut defined = HashMap::new();
        let mut seen_defined = HashMap::new();
        if input.is_empty() {
            return Ok(Self {
                endpoint,
                source,
                parameters,
                defined,
            });
        }

        let mut ordinal = 0;
        for pair in input.split(|byte| *byte == b'&') {
            // HTML form serialization permits empty segments, such as a
            // leading/trailing ampersand or `&&`. They carry no parameter and
            // must not consume the bounded admitted-pair budget.
            if pair.is_empty() {
                continue;
            }
            if ordinal >= MAX_OAUTH_PARAMETER_PAIRS {
                return Err(OAuthParameterAdmissionError::TooManyPairs);
            }
            // In application/x-www-form-urlencoded, both `name` and `name=`
            // mean an empty value. Required-field validation occurs only
            // after this layer turns either spelling into an omitted typed
            // defined value.
            let (raw_name, raw_value): (&[u8], &[u8]) =
                match pair.iter().position(|byte| *byte == b'=') {
                    Some(delimiter) => {
                        let (name, value_with_delimiter) = pair.split_at(delimiter);
                        (name, &value_with_delimiter[1..])
                    }
                    None => (pair, &[]),
                };
            let name = Zeroizing::new(
                decode_oauth_form_component(raw_name, MAX_OAUTH_PARAMETER_NAME_BYTES).map_err(
                    |error| match error {
                        OAuthFormDecodeError::TooLarge => {
                            OAuthParameterAdmissionError::NameTooLarge
                        }
                        error => OAuthParameterAdmissionError::from(error),
                    },
                )?,
            );
            let value = Zeroizing::new(
                decode_oauth_form_component(raw_value, MAX_OAUTH_PARAMETER_VALUE_BYTES)
                    .map_err(OAuthParameterAdmissionError::from)?,
            );
            if name.is_empty() {
                return Err(OAuthParameterAdmissionError::EmptyName);
            }
            if name.chars().any(char::is_control) || value.chars().any(char::is_control) {
                return Err(OAuthParameterAdmissionError::ControlCharacter);
            }

            let value_len = value.len();
            let defined_name = endpoint.defined_parameter(name.as_str());
            if let Some(defined_name) = defined_name {
                if let Some(first_ordinal) = seen_defined.insert(defined_name, ordinal) {
                    return Err(OAuthParameterAdmissionError::DuplicateDefinedParameter {
                        parameter: defined_name,
                        source,
                        first_ordinal,
                        duplicate_ordinal: ordinal,
                    });
                }
                // An empty defined field is intentionally equivalent to an
                // omitted field for endpoint parsing, while still counting as
                // present for duplicate-pollution rejection.
                if !value.is_empty() {
                    defined.insert(defined_name, OAuthSensitiveParameterValue { value });
                }
            }
            parameters.push(OAuthAdmittedParameter {
                source,
                ordinal,
                // Names are the intentionally public diagnostic surface; all
                // decoded staging buffers remain owned by `Zeroizing`.
                name: name.to_string(),
                value_len,
                defined: defined_name.is_some(),
            });
            ordinal += 1;
        }

        Ok(Self {
            endpoint,
            source,
            parameters,
            defined,
        })
    }

    /// Returns the selected endpoint profile.
    #[must_use]
    pub const fn endpoint(&self) -> OAuthParameterEndpoint {
        self.endpoint
    }

    /// Returns the sole wire source permitted by the selected profile.
    #[must_use]
    pub const fn source(&self) -> OAuthParameterSource {
        self.source
    }

    /// Returns every admitted parameter in exact decoded wire order.
    #[must_use]
    pub fn parameters(&self) -> &[OAuthAdmittedParameter] {
        &self.parameters
    }

    /// Removes one nonempty endpoint-defined value for crate-local typed
    /// endpoint parsing.
    ///
    /// An empty defined value is omitted, and unknown values are dropped after
    /// validation instead of being retained. Taking is one-way: no public API
    /// can inspect, clone, or serialize these values.
    pub(crate) fn take_defined_value(
        &mut self,
        name: OAuthParameterName,
    ) -> Option<OAuthSensitiveParameterValue> {
        self.defined.remove(&name)
    }

    /// Iterates bounded unknown parameters in their original wire order.
    ///
    /// Their decoded values are zeroized immediately after admission and are
    /// not retained by this result.
    pub fn unknown_parameters(&self) -> impl Iterator<Item = &OAuthAdmittedParameter> {
        self.parameters
            .iter()
            .filter(|parameter| !parameter.is_defined())
    }
}

pub(crate) enum OAuthFormDecodeError {
    TooLarge,
    MalformedPercentEncoding,
    InvalidUtf8,
}

impl From<OAuthFormDecodeError> for OAuthParameterAdmissionError {
    fn from(error: OAuthFormDecodeError) -> Self {
        match error {
            OAuthFormDecodeError::TooLarge => Self::ValueTooLarge,
            OAuthFormDecodeError::MalformedPercentEncoding => Self::MalformedPercentEncoding,
            OAuthFormDecodeError::InvalidUtf8 => Self::InvalidUtf8,
        }
    }
}

pub(crate) fn decode_oauth_form_component(
    input: &[u8],
    maximum_output_bytes: usize,
) -> Result<String, OAuthFormDecodeError> {
    // Keep the wire-decoded scratch bytes in zeroizing storage on every
    // return path. In particular, malformed percent escapes, invalid UTF-8,
    // and output-limit rejection must not leave a temporary credential copy
    // in an ordinary dropped allocation.
    let mut decoded = Zeroizing::new(Vec::with_capacity(input.len().min(maximum_output_bytes)));
    let mut index = 0;
    while index < input.len() {
        let byte = input[index];
        match byte {
            b'+' => {
                decoded.push(b' ');
                index += 1;
            }
            b'%' => {
                let Some(high) = input.get(index + 1).copied().and_then(decode_hex_digit) else {
                    return Err(OAuthFormDecodeError::MalformedPercentEncoding);
                };
                let Some(low) = input.get(index + 2).copied().and_then(decode_hex_digit) else {
                    return Err(OAuthFormDecodeError::MalformedPercentEncoding);
                };
                decoded.push((high << 4) | low);
                index += 3;
            }
            _ => {
                decoded.push(byte);
                index += 1;
            }
        }
        if decoded.len() > maximum_output_bytes {
            return Err(OAuthFormDecodeError::TooLarge);
        }
    }
    // Do not move the scratch allocation into `String`: copying the validated
    // text leaves `decoded` owned by `Zeroizing`, which wipes it on both the
    // success and error paths. Defined values are immediately wrapped in their
    // own one-way zeroizing holder by the caller; unknown values are dropped.
    std::str::from_utf8(decoded.as_slice())
        .map(str::to_owned)
        .map_err(|_| OAuthFormDecodeError::InvalidUtf8)
}

const fn decode_hex_digit(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

// These ceilings make accidentally persistent bearer credentials and
// authorization codes fail closed at configuration admission. Deployments
// needing longer-lived sessions should rotate refresh tokens instead of
// extending access-token or authorization-code exposure.
const MAX_ACCESS_TOKEN_LIFETIME: Duration = Duration::from_hours(24);
const MAX_REFRESH_TOKEN_LIFETIME: Duration = Duration::from_hours(8_760);
const MAX_AUTHORIZATION_CODE_LIFETIME: Duration = Duration::from_mins(10);
const MIN_OAUTH_CREDENTIAL_LIFETIME: Duration = Duration::from_secs(1);

/// Default maximum number of registered OAuth clients.
pub const DEFAULT_MAX_OAUTH_CLIENTS: usize = 1_024;
/// Default maximum number of pending authorization codes.
pub const DEFAULT_MAX_AUTHORIZATION_CODES: usize = 16 * 1_024;
/// Default maximum pending authorization codes for one client.
pub const DEFAULT_MAX_AUTHORIZATION_CODES_PER_CLIENT: usize = 64;
/// Default maximum number of active access tokens.
pub const DEFAULT_MAX_ACCESS_TOKENS: usize = 64 * 1_024;
/// Default maximum active access tokens for one client.
pub const DEFAULT_MAX_ACCESS_TOKENS_PER_CLIENT: usize = 256;
/// Default maximum number of active refresh tokens.
pub const DEFAULT_MAX_REFRESH_TOKENS: usize = 16 * 1_024;
/// Default maximum active refresh tokens for one client.
pub const DEFAULT_MAX_REFRESH_TOKENS_PER_CLIENT: usize = 64;
/// Default maximum number of retained revocation tombstones.
pub const DEFAULT_MAX_REVOCATION_TOMBSTONES: usize = 64 * 1_024;
/// Default maximum retained revocation tombstones for one client.
pub const DEFAULT_MAX_REVOCATION_TOMBSTONES_PER_CLIENT: usize = 256;

/// Hard configuration ceiling for registered OAuth clients.
pub const HARD_MAX_OAUTH_CLIENTS: usize = 4 * DEFAULT_MAX_OAUTH_CLIENTS;
/// Hard configuration ceiling for pending authorization codes.
pub const HARD_MAX_AUTHORIZATION_CODES: usize = 2 * DEFAULT_MAX_AUTHORIZATION_CODES;
/// Hard per-client configuration ceiling for pending authorization codes.
pub const HARD_MAX_AUTHORIZATION_CODES_PER_CLIENT: usize =
    4 * DEFAULT_MAX_AUTHORIZATION_CODES_PER_CLIENT;
/// Hard configuration ceiling for active access tokens.
pub const HARD_MAX_ACCESS_TOKENS: usize = 2 * DEFAULT_MAX_ACCESS_TOKENS;
/// Hard per-client configuration ceiling for active access tokens.
pub const HARD_MAX_ACCESS_TOKENS_PER_CLIENT: usize = 4 * DEFAULT_MAX_ACCESS_TOKENS_PER_CLIENT;
/// Hard configuration ceiling for active refresh tokens.
pub const HARD_MAX_REFRESH_TOKENS: usize = 2 * DEFAULT_MAX_REFRESH_TOKENS;
/// Hard per-client configuration ceiling for active refresh tokens.
pub const HARD_MAX_REFRESH_TOKENS_PER_CLIENT: usize = 4 * DEFAULT_MAX_REFRESH_TOKENS_PER_CLIENT;
/// Hard configuration ceiling for retained revocation tombstones.
pub const HARD_MAX_REVOCATION_TOMBSTONES: usize = 2 * DEFAULT_MAX_REVOCATION_TOMBSTONES;
/// Hard per-client configuration ceiling for retained revocation tombstones.
pub const HARD_MAX_REVOCATION_TOMBSTONES_PER_CLIENT: usize =
    4 * DEFAULT_MAX_REVOCATION_TOMBSTONES_PER_CLIENT;
/// Hard aggregate ceiling across every globally retained OAuth state map.
///
/// This covers registered clients, pending authorization codes, live access
/// and refresh tokens, and revocation tombstones. Per-client limits partition
/// those same global entries and therefore are not added a second time.
pub const HARD_MAX_OAUTH_RETAINED_ENTRIES: usize = 256 * 1_024;

// =============================================================================
// Configuration
// =============================================================================

/// Configuration for the OAuth authorization server.
#[derive(Debug, Clone)]
pub struct OAuthServerConfig {
    /// Issuer identifier (URL) for this authorization server.
    pub issuer: String,
    /// Access token lifetime.
    pub access_token_lifetime: Duration,
    /// Refresh token lifetime.
    pub refresh_token_lifetime: Duration,
    /// Authorization code lifetime (should be short; the default is 5 minutes).
    pub authorization_code_lifetime: Duration,
    /// Whether to allow public clients (clients without a secret).
    pub allow_public_clients: bool,
    /// Minimum PKCE code verifier length (default: 43, min: 43, max: 128).
    pub min_code_verifier_length: usize,
    /// Maximum PKCE code verifier length.
    pub max_code_verifier_length: usize,
    /// Maximum number of registered clients (hard-capped by
    /// [`HARD_MAX_OAUTH_CLIENTS`]).
    pub max_clients: usize,
    /// Maximum number of pending authorization codes globally (hard-capped by
    /// [`HARD_MAX_AUTHORIZATION_CODES`]).
    pub max_authorization_codes: usize,
    /// Maximum number of pending authorization codes for one client
    /// (hard-capped by [`HARD_MAX_AUTHORIZATION_CODES_PER_CLIENT`]).
    pub max_authorization_codes_per_client: usize,
    /// Maximum number of active access tokens globally (hard-capped by
    /// [`HARD_MAX_ACCESS_TOKENS`]).
    pub max_access_tokens: usize,
    /// Maximum number of active access tokens for one client (hard-capped by
    /// [`HARD_MAX_ACCESS_TOKENS_PER_CLIENT`]).
    pub max_access_tokens_per_client: usize,
    /// Maximum number of active refresh tokens globally (hard-capped by
    /// [`HARD_MAX_REFRESH_TOKENS`]).
    pub max_refresh_tokens: usize,
    /// Maximum number of active refresh tokens for one client (hard-capped by
    /// [`HARD_MAX_REFRESH_TOKENS_PER_CLIENT`]).
    pub max_refresh_tokens_per_client: usize,
    /// Maximum number of revocation tombstones globally (hard-capped by
    /// [`HARD_MAX_REVOCATION_TOMBSTONES`]).
    pub max_revocation_tombstones: usize,
    /// Maximum number of revocation tombstones for one client (hard-capped by
    /// [`HARD_MAX_REVOCATION_TOMBSTONES_PER_CLIENT`]).
    pub max_revocation_tombstones_per_client: usize,
}

impl Default for OAuthServerConfig {
    fn default() -> Self {
        Self {
            // `.invalid` is reserved for names that must never resolve. This is
            // syntactically safe while still making deployment configuration
            // visibly mandatory for interoperable issuer claims.
            issuer: "https://fastmcp.invalid/".to_string(),
            access_token_lifetime: Duration::from_mins(15),
            refresh_token_lifetime: Duration::from_hours(720),
            authorization_code_lifetime: Duration::from_mins(5),
            allow_public_clients: true,
            min_code_verifier_length: PKCE_CODE_VERIFIER_MIN_BYTES,
            max_code_verifier_length: PKCE_CODE_VERIFIER_MAX_BYTES,
            max_clients: DEFAULT_MAX_OAUTH_CLIENTS,
            max_authorization_codes: DEFAULT_MAX_AUTHORIZATION_CODES,
            max_authorization_codes_per_client: DEFAULT_MAX_AUTHORIZATION_CODES_PER_CLIENT,
            max_access_tokens: DEFAULT_MAX_ACCESS_TOKENS,
            max_access_tokens_per_client: DEFAULT_MAX_ACCESS_TOKENS_PER_CLIENT,
            max_refresh_tokens: DEFAULT_MAX_REFRESH_TOKENS,
            max_refresh_tokens_per_client: DEFAULT_MAX_REFRESH_TOKENS_PER_CLIENT,
            max_revocation_tombstones: DEFAULT_MAX_REVOCATION_TOMBSTONES,
            max_revocation_tombstones_per_client: DEFAULT_MAX_REVOCATION_TOMBSTONES_PER_CLIENT,
        }
    }
}

impl OAuthServerConfig {
    /// Validates PKCE policy, state-retention limits, and token lifetimes.
    ///
    /// # Errors
    ///
    /// Returns an error when PKCE bounds are outside RFC 7636, a state cap is
    /// zero, above its hard ceiling, incoherent, or over the checked aggregate
    /// retention ceiling, or a configured lifetime is zero or excessive.
    pub fn validate(&self) -> Result<(), OAuthError> {
        validate_oauth_issuer(&self.issuer)?;

        if !(PKCE_CODE_VERIFIER_MIN_BYTES..=PKCE_CODE_VERIFIER_MAX_BYTES)
            .contains(&self.min_code_verifier_length)
            || !(PKCE_CODE_VERIFIER_MIN_BYTES..=PKCE_CODE_VERIFIER_MAX_BYTES)
                .contains(&self.max_code_verifier_length)
            || self.min_code_verifier_length > self.max_code_verifier_length
        {
            return Err(OAuthError::ServerError(format!(
                "OAuth configuration PKCE verifier bounds must satisfy \
                 {PKCE_CODE_VERIFIER_MIN_BYTES} <= min_code_verifier_length <= \
                 max_code_verifier_length <= {PKCE_CODE_VERIFIER_MAX_BYTES}"
            )));
        }

        for (field, limit, hard_limit) in [
            ("max_clients", self.max_clients, HARD_MAX_OAUTH_CLIENTS),
            (
                "max_authorization_codes",
                self.max_authorization_codes,
                HARD_MAX_AUTHORIZATION_CODES,
            ),
            (
                "max_authorization_codes_per_client",
                self.max_authorization_codes_per_client,
                HARD_MAX_AUTHORIZATION_CODES_PER_CLIENT,
            ),
            (
                "max_access_tokens",
                self.max_access_tokens,
                HARD_MAX_ACCESS_TOKENS,
            ),
            (
                "max_access_tokens_per_client",
                self.max_access_tokens_per_client,
                HARD_MAX_ACCESS_TOKENS_PER_CLIENT,
            ),
            (
                "max_refresh_tokens",
                self.max_refresh_tokens,
                HARD_MAX_REFRESH_TOKENS,
            ),
            (
                "max_refresh_tokens_per_client",
                self.max_refresh_tokens_per_client,
                HARD_MAX_REFRESH_TOKENS_PER_CLIENT,
            ),
            (
                "max_revocation_tombstones",
                self.max_revocation_tombstones,
                HARD_MAX_REVOCATION_TOMBSTONES,
            ),
            (
                "max_revocation_tombstones_per_client",
                self.max_revocation_tombstones_per_client,
                HARD_MAX_REVOCATION_TOMBSTONES_PER_CLIENT,
            ),
        ] {
            if !(1..=hard_limit).contains(&limit) {
                return Err(OAuthError::ServerError(format!(
                    "OAuth configuration limit `{field}` must be between 1 and its hard ceiling \
                     of {hard_limit}"
                )));
            }
        }

        let retained_entries = self.checked_global_retention_limit()?;
        if retained_entries > HARD_MAX_OAUTH_RETAINED_ENTRIES {
            return Err(OAuthError::ServerError(format!(
                "OAuth aggregate retained-state limit {retained_entries} exceeds hard ceiling \
                 {HARD_MAX_OAUTH_RETAINED_ENTRIES}"
            )));
        }

        for (global_field, global_limit, per_client_field, per_client_limit) in [
            (
                "max_authorization_codes",
                self.max_authorization_codes,
                "max_authorization_codes_per_client",
                self.max_authorization_codes_per_client,
            ),
            (
                "max_access_tokens",
                self.max_access_tokens,
                "max_access_tokens_per_client",
                self.max_access_tokens_per_client,
            ),
            (
                "max_refresh_tokens",
                self.max_refresh_tokens,
                "max_refresh_tokens_per_client",
                self.max_refresh_tokens_per_client,
            ),
            (
                "max_revocation_tombstones",
                self.max_revocation_tombstones,
                "max_revocation_tombstones_per_client",
                self.max_revocation_tombstones_per_client,
            ),
        ] {
            if per_client_limit > global_limit {
                return Err(OAuthError::ServerError(format!(
                    "OAuth configuration limit `{per_client_field}` must not exceed \
                     `{global_field}`"
                )));
            }
        }

        validate_lifetime(
            self.access_token_lifetime,
            MIN_OAUTH_CREDENTIAL_LIFETIME,
            MAX_ACCESS_TOKEN_LIFETIME,
            "access_token_lifetime",
        )?;
        validate_lifetime(
            self.refresh_token_lifetime,
            MIN_OAUTH_CREDENTIAL_LIFETIME,
            MAX_REFRESH_TOKEN_LIFETIME,
            "refresh_token_lifetime",
        )?;
        validate_lifetime(
            self.authorization_code_lifetime,
            MIN_OAUTH_CREDENTIAL_LIFETIME,
            MAX_AUTHORIZATION_CODE_LIFETIME,
            "authorization_code_lifetime",
        )?;
        if self.refresh_token_lifetime < self.access_token_lifetime {
            return Err(OAuthError::ServerError(
                "OAuth configuration `refresh_token_lifetime` must not be shorter than \
                 `access_token_lifetime`"
                    .to_string(),
            ));
        }

        let now = Instant::now();
        checked_deadline(now, self.access_token_lifetime, "access_token_lifetime")?;
        checked_deadline(now, self.refresh_token_lifetime, "refresh_token_lifetime")?;
        checked_deadline(
            now,
            self.authorization_code_lifetime,
            "authorization_code_lifetime",
        )?;
        Ok(())
    }

    fn checked_global_retention_limit(&self) -> Result<usize, OAuthError> {
        [
            self.max_clients,
            self.max_authorization_codes,
            self.max_access_tokens,
            self.max_refresh_tokens,
            self.max_revocation_tombstones,
        ]
        .into_iter()
        .try_fold(0_usize, |total, limit| {
            total.checked_add(limit).ok_or_else(|| {
                OAuthError::ServerError(
                    "OAuth aggregate retained-state limit is not representable".to_string(),
                )
            })
        })
    }
}

// =============================================================================
// OAuth Client
// =============================================================================

/// OAuth client types.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientType {
    /// Confidential client (has a secret).
    Confidential,
    /// Public client (no secret, e.g., native apps, SPAs).
    Public,
}

/// A confidential-client credential retained only until registration.
///
/// The bytes are zeroized when the input object is dropped. Registered server
/// state contains only a verifier, never this plaintext value.
#[derive(Zeroize, ZeroizeOnDrop)]
struct ClientSecret {
    bytes: Vec<u8>,
}

impl ClientSecret {
    fn new(value: String) -> Self {
        Self {
            bytes: value.into_bytes(),
        }
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl std::fmt::Debug for ClientSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecret([redacted])")
    }
}

/// Bounded salted verifier used by the current in-memory development server.
///
/// It removes plaintext retention and provides fixed-width comparison, but it
/// is not the AUTH-06 Argon2id production verifier. Production promotion still
/// requires the admitted blocking-work/KDF provider described by the plan.
#[derive(Clone, Copy)]
struct ClientSecretVerifier {
    salt: [u8; CLIENT_SECRET_SALT_BYTES],
    digest: Sha256Digest,
}

impl ClientSecretVerifier {
    fn create(secret: &ClientSecret) -> Result<Self, OAuthError> {
        let salt = draw_security_identifier()
            .map_err(|error| OAuthError::ServerError(error.to_string()))?;
        Self::create_with_salt(secret.as_bytes(), *salt.as_bytes())
    }

    fn create_with_salt(
        secret: &[u8],
        salt: [u8; CLIENT_SECRET_SALT_BYTES],
    ) -> Result<Self, OAuthError> {
        let digest = client_secret_digest(&salt, secret)?;
        Ok(Self { salt, digest })
    }

    fn dummy() -> Self {
        Self {
            salt: DUMMY_CLIENT_SECRET_SALT,
            digest: DUMMY_CLIENT_SECRET_DIGEST,
        }
    }

    fn verify(&self, provided: &[u8]) -> bool {
        client_secret_digest(&self.salt, provided).is_ok_and(|provided_digest| {
            constant_time_digest_eq(self.digest.as_bytes(), provided_digest.as_bytes())
        })
    }
}

impl std::fmt::Debug for ClientSecretVerifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientSecretVerifier([redacted])")
    }
}

/// A registered OAuth client.
pub struct OAuthClient {
    /// Unique client identifier.
    pub client_id: String,
    /// Client secret supplied for registration (absent for public clients).
    ///
    /// This field is intentionally private and cannot be cloned or recovered.
    /// [`OAuthServer::register_client`] consumes it and retains only a verifier.
    client_secret: Option<ClientSecret>,
    /// Client type.
    pub client_type: ClientType,
    /// Allowed redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Allowed scopes.
    pub allowed_scopes: HashSet<String>,
    /// Client name (for display).
    pub name: Option<String>,
    /// Client description.
    pub description: Option<String>,
    /// When the client was registered.
    pub registered_at: SystemTime,
}

struct RegisteredOAuthClient {
    metadata: OAuthClientMetadata,
    secret_verifier: Option<ClientSecretVerifier>,
    registration_epoch: OAuthRegistrationEpoch,
}

impl RegisteredOAuthClient {
    fn from_registration(
        client: OAuthClient,
        registration_epoch: OAuthRegistrationEpoch,
    ) -> Result<Self, OAuthError> {
        client.validate_for_retention()?;
        let secret_verifier = client
            .client_secret
            .as_ref()
            .map(ClientSecretVerifier::create)
            .transpose()?;
        let metadata = OAuthClientMetadata::from(&client);
        Ok(Self {
            metadata,
            secret_verifier,
            registration_epoch,
        })
    }

    fn validate_redirect_uri(&self, uri: &str) -> bool {
        self.metadata.validate_redirect_uri(uri)
    }

    fn validate_scopes(&self, scopes: &[String]) -> bool {
        self.metadata.validate_scopes(scopes)
    }

    fn authenticate(&self, provided: Option<&str>) -> bool {
        match (self.secret_verifier.as_ref(), provided) {
            (Some(verifier), Some(secret)) => verifier.verify(secret.as_bytes()),
            (Some(verifier), None) => {
                std::hint::black_box(verifier.verify(std::hint::black_box(&[])));
                false
            }
            (None, None) => {
                perform_dummy_client_secret_verification(&[]);
                self.metadata.client_type == ClientType::Public
            }
            (None, Some(secret)) => {
                perform_dummy_client_secret_verification(secret.as_bytes());
                false
            }
        }
    }
}

/// Non-reusable identity for one registration of an OAuth client ID.
///
/// A client ID may be registered again only after unregistration. Keeping this
/// epoch on codes and token families prevents an old authorization decision
/// from being transferred to the new registration through that ABA sequence.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct OAuthRegistrationEpoch([u8; OAUTH_REGISTRATION_EPOCH_BYTES]);

impl OAuthRegistrationEpoch {
    fn draw() -> Result<Self, OAuthError> {
        let identifier = draw_security_identifier()
            .map_err(|error| OAuthError::ServerError(error.to_string()))?;
        Ok(Self(*identifier.as_bytes()))
    }

    const fn as_bytes(&self) -> &[u8; OAUTH_REGISTRATION_EPOCH_BYTES] {
        &self.0
    }
}

impl std::fmt::Debug for OAuthRegistrationEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuthRegistrationEpoch([opaque; 32 bytes])")
    }
}

/// Public metadata for a registered OAuth client.
///
/// This is the administrative read model returned by [`OAuthServer::get_client`]
/// and [`OAuthServer::list_clients`]. It deliberately has no client-credential
/// field: a confidential client's secret is available only to the registration
/// flow that creates or receives the [`OAuthClient`].
#[derive(Clone, PartialEq, Eq)]
pub struct OAuthClientMetadata {
    /// Unique client identifier.
    pub client_id: String,
    /// Client credential classification.
    pub client_type: ClientType,
    /// Allowed redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Allowed scopes.
    pub allowed_scopes: HashSet<String>,
    /// Client name (for display).
    pub name: Option<String>,
    /// Client description.
    pub description: Option<String>,
    /// When the client was registered.
    pub registered_at: SystemTime,
}

impl std::fmt::Debug for OAuthClientMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClientMetadata")
            .field("client_id_len", &self.client_id.len())
            .field("client_type", &self.client_type)
            .field("redirect_uri_count", &self.redirect_uris.len())
            .field("allowed_scope_count", &self.allowed_scopes.len())
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .finish_non_exhaustive()
    }
}

impl From<&OAuthClient> for OAuthClientMetadata {
    fn from(client: &OAuthClient) -> Self {
        Self {
            client_id: client.client_id.clone(),
            client_type: client.client_type,
            redirect_uris: client.redirect_uris.clone(),
            allowed_scopes: client.allowed_scopes.clone(),
            name: client.name.clone(),
            description: client.description.clone(),
            registered_at: client.registered_at,
        }
    }
}

impl From<&RegisteredOAuthClient> for OAuthClientMetadata {
    fn from(client: &RegisteredOAuthClient) -> Self {
        client.metadata.clone()
    }
}

impl OAuthClientMetadata {
    /// Validates that a redirect URI is allowed for this client.
    #[must_use]
    pub fn validate_redirect_uri(&self, uri: &str) -> bool {
        validate_registered_redirect_uri(&self.redirect_uris, uri)
    }

    /// Validates that the requested scopes are allowed for this client.
    #[must_use]
    pub fn validate_scopes(&self, scopes: &[String]) -> bool {
        validate_registered_scopes(&self.allowed_scopes, scopes)
    }
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id_len", &self.client_id.len())
            .field("client_secret_present", &self.client_secret.is_some())
            .field("redirect_uri_count", &self.redirect_uris.len())
            .field("allowed_scope_count", &self.allowed_scopes.len())
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .finish_non_exhaustive()
    }
}

impl OAuthClient {
    /// Creates a new client builder.
    #[must_use]
    pub fn builder(client_id: impl Into<String>) -> OAuthClientBuilder {
        OAuthClientBuilder::new(client_id)
    }

    fn validate_for_retention(&self) -> Result<(), OAuthError> {
        if self.client_id.is_empty()
            || self.client_id.len() > MAX_OAUTH_CLIENT_ID_BYTES
            || self.client_id.chars().any(char::is_control)
        {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_ID_RETENTION_ERROR.to_string(),
            ));
        }

        if self.client_secret.as_ref().is_some_and(|credential| {
            credential.as_bytes().is_empty()
                || credential.as_bytes().len() > MAX_OAUTH_CLIENT_CREDENTIAL_BYTES
        }) {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_CREDENTIAL_RETENTION_ERROR.to_string(),
            ));
        }

        let credential_class_is_consistent = matches!(
            (self.client_type, self.client_secret.is_some()),
            (ClientType::Public, false) | (ClientType::Confidential, true)
        );
        if !credential_class_is_consistent {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_CREDENTIAL_CLASS_ERROR.to_string(),
            ));
        }

        if self.redirect_uris.is_empty() {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_REDIRECT_REQUIRED_ERROR.to_string(),
            ));
        }
        if self.redirect_uris.len() > MAX_OAUTH_REDIRECT_URIS_PER_CLIENT {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_REDIRECT_COUNT_ERROR.to_string(),
            ));
        }
        if self
            .redirect_uris
            .iter()
            .any(|uri| parse_redirect_uri(uri).is_none())
        {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_REDIRECT_VALUE_ERROR.to_string(),
            ));
        }

        if self.allowed_scopes.len() > MAX_OAUTH_SCOPES_PER_CLIENT {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_SCOPE_COUNT_ERROR.to_string(),
            ));
        }
        if self
            .allowed_scopes
            .iter()
            .any(|scope| !is_valid_oauth_scope_token(scope))
        {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_SCOPE_VALUE_ERROR.to_string(),
            ));
        }

        if self.name.as_ref().is_some_and(|name| {
            name.len() > MAX_OAUTH_CLIENT_NAME_BYTES || contains_unsafe_display_character(name)
        }) {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_NAME_RETENTION_ERROR.to_string(),
            ));
        }
        if self.description.as_ref().is_some_and(|description| {
            description.len() > MAX_OAUTH_CLIENT_DESCRIPTION_BYTES
                || contains_unsafe_display_character(description)
        }) {
            return Err(OAuthError::InvalidRequest(
                OAUTH_CLIENT_DESCRIPTION_RETENTION_ERROR.to_string(),
            ));
        }

        Ok(())
    }

    /// Validates that a redirect URI is allowed for this client.
    #[must_use]
    pub fn validate_redirect_uri(&self, uri: &str) -> bool {
        validate_registered_redirect_uri(&self.redirect_uris, uri)
    }

    /// Validates that the requested scopes are allowed for this client.
    #[must_use]
    pub fn validate_scopes(&self, scopes: &[String]) -> bool {
        validate_registered_scopes(&self.allowed_scopes, scopes)
    }

    /// Authenticates a confidential client.
    #[must_use]
    pub fn authenticate(&self, secret: Option<&str>) -> bool {
        match (&self.client_secret, secret) {
            (Some(expected), Some(provided)) => {
                authenticate_client_secret(expected.as_bytes(), provided.as_bytes())
            }
            (None, None) => self.client_type == ClientType::Public,
            _ => false,
        }
    }

    /// Returns whether this registration input carries a confidential-client
    /// credential.
    #[must_use]
    pub fn has_client_secret(&self) -> bool {
        self.client_secret.is_some()
    }
}

/// Builder for OAuth clients.
pub struct OAuthClientBuilder {
    client_id: String,
    client_credential: Option<ClientSecret>,
    redirect_uris: Vec<String>,
    allowed_scopes: HashSet<String>,
    name: Option<String>,
    description: Option<String>,
}

impl std::fmt::Debug for OAuthClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClientBuilder")
            .field("client_id_len", &self.client_id.len())
            .field(
                "client_credential_present",
                &self.client_credential.is_some(),
            )
            .field("redirect_uri_count", &self.redirect_uris.len())
            .field("allowed_scope_count", &self.allowed_scopes.len())
            .field("name_present", &self.name.is_some())
            .field("description_present", &self.description.is_some())
            .finish()
    }
}

impl OAuthClientBuilder {
    /// Creates a new client builder.
    fn new(client_id: impl Into<String>) -> Self {
        Self {
            client_id: client_id.into(),
            client_credential: None,
            redirect_uris: Vec::new(),
            allowed_scopes: HashSet::new(),
            name: None,
            description: None,
        }
    }

    /// Sets the client secret (makes this a confidential client).
    #[must_use]
    pub fn secret(mut self, credential: impl Into<String>) -> Self {
        self.client_credential = Some(ClientSecret::new(credential.into()));
        self
    }

    /// Adds a redirect URI.
    #[must_use]
    pub fn redirect_uri(mut self, uri: impl Into<String>) -> Self {
        self.redirect_uris.push(uri.into());
        self
    }

    /// Adds multiple redirect URIs.
    #[must_use]
    pub fn redirect_uris<I, S>(mut self, uris: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.redirect_uris.extend(uris.into_iter().map(Into::into));
        self
    }

    /// Adds an allowed scope.
    #[must_use]
    pub fn scope(mut self, scope: impl Into<String>) -> Self {
        self.allowed_scopes.insert(scope.into());
        self
    }

    /// Adds multiple allowed scopes.
    #[must_use]
    pub fn scopes<I, S>(mut self, scopes: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.allowed_scopes
            .extend(scopes.into_iter().map(Into::into));
        self
    }

    /// Sets the client name.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    /// Sets the client description.
    #[must_use]
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Builds the OAuth client.
    ///
    /// # Errors
    ///
    /// Returns an error if the client metadata is empty, inconsistent, or
    /// exceeds a retained-value or retained-count bound.
    pub fn build(self) -> Result<OAuthClient, OAuthError> {
        let client_type = if self.client_credential.is_some() {
            ClientType::Confidential
        } else {
            ClientType::Public
        };

        let client = OAuthClient {
            client_id: self.client_id,
            client_secret: self.client_credential,
            client_type,
            redirect_uris: self.redirect_uris,
            allowed_scopes: self.allowed_scopes,
            name: self.name,
            description: self.description,
            registered_at: SystemTime::now(),
        };
        client.validate_for_retention()?;
        Ok(client)
    }
}

/// Strict RFC 7591 subset accepted by the built-in registration route.
///
/// The route intentionally creates native public clients only. Confidential
/// registration, software statements, registration management credentials and
/// arbitrary extension metadata require a separately reviewed profile.
#[derive(Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NativePublicClientRegistrationRequest {
    pub client_name: String,
    pub application_type: String,
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: String,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Exact public response consumed by FastMCP's native DCR client.
#[derive(serde::Serialize)]
pub(crate) struct NativePublicClientRegistrationResponse {
    pub client_id: String,
    pub client_name: String,
    pub application_type: &'static str,
    pub redirect_uris: Vec<String>,
    pub token_endpoint_auth_method: &'static str,
    pub grant_types: Vec<String>,
    pub response_types: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

// =============================================================================
// Authorization Code
// =============================================================================

/// PKCE code challenge method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodeChallengeMethod {
    /// Legacy plain-text method.
    ///
    /// This value can be parsed so callers can return a precise protocol
    /// error, but [`OAuthServer`] rejects it for authorization-code grants.
    Plain,
    /// SHA-256 hash (required by this OAuth 2.1 authorization-code path).
    S256,
}

impl CodeChallengeMethod {
    /// Parses a code challenge method from a string.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "plain" => Some(Self::Plain),
            "S256" => Some(Self::S256),
            _ => None,
        }
    }

    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::S256 => "S256",
        }
    }
}

/// Metadata retained for an authorization code issued during the flow.
///
/// The raw code is returned once by [`OAuthServer::authorize`]; server state
/// indexes this metadata by a domain-separated digest and never retains the
/// raw credential.
#[derive(Clone)]
pub struct AuthorizationCode {
    /// Client ID this code was issued to.
    pub client_id: String,
    /// Redirect URI used in the authorization request.
    pub redirect_uri: String,
    /// Approved scopes.
    pub scopes: Vec<String>,
    /// Approved RFC 8707 resource indicator, if one was requested.
    pub resource: Option<String>,
    /// PKCE code challenge.
    pub code_challenge: String,
    /// PKCE code challenge method.
    pub code_challenge_method: CodeChallengeMethod,
    /// When the code was issued.
    pub issued_at: Instant,
    /// When the code expires.
    pub expires_at: Instant,
    /// Subject (user) this code was issued for.
    pub subject: Option<String>,
    /// State parameter from the authorization request.
    pub state: Option<String>,
    /// Exact client registration that received the authorization decision.
    registration_epoch: OAuthRegistrationEpoch,
}

impl std::fmt::Debug for AuthorizationCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationCode")
            .field("client_id_len", &self.client_id.len())
            .field("redirect_uri_len", &self.redirect_uri.len())
            .field("scope_count", &self.scopes.len())
            .field("resource_present", &self.resource.is_some())
            .field("code_challenge_len", &self.code_challenge.len())
            .field("subject_present", &self.subject.is_some())
            .field("state_present", &self.state.is_some())
            .finish_non_exhaustive()
    }
}

impl AuthorizationCode {
    /// Checks if this code has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Validates the PKCE code verifier against the stored challenge.
    #[must_use]
    pub fn validate_code_verifier(&self, verifier: &str) -> bool {
        if validate_pkce_code_verifier(verifier).is_err() {
            return false;
        }

        match self.code_challenge_method {
            CodeChallengeMethod::Plain => false,
            CodeChallengeMethod::S256 => compute_s256_challenge(verifier)
                .is_ok_and(|computed| constant_time_eq(&self.code_challenge, &computed)),
        }
    }
}

// =============================================================================
// OAuth Tokens
// =============================================================================

/// Token type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenType {
    /// Bearer token.
    Bearer,
}

impl TokenType {
    /// Returns the string representation.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Bearer => "bearer",
        }
    }
}

/// Fixed-width, non-secret identifier for one authorization grant family.
///
/// Rotation preserves this identifier so replay or explicit refresh-token
/// revocation can invalidate every descendant without retaining raw bearer
/// credentials.
#[repr(transparent)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct OAuthGrantId([u8; 32]);

impl OAuthGrantId {
    /// Constructs a non-secret grant-family identifier from its fixed-width
    /// representation.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// Borrows the fixed-width grant-family identifier.
    #[must_use]
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for OAuthGrantId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("OAuthGrantId([opaque; 32 bytes])")
    }
}

/// OAuth token (access or refresh).
///
/// This value contains token metadata only. Raw bearer credentials are never
/// retained in server state and therefore cannot be recovered through token
/// introspection.
#[derive(Clone)]
pub struct OAuthToken {
    /// Auxiliary token text supplied by callers constructing standalone token
    /// metadata.
    ///
    /// Server-issued and introspected values always leave this empty; raw
    /// bearer credentials are retained only by the one-shot [`TokenResponse`].
    pub token: String,
    /// Token type.
    pub token_type: TokenType,
    /// Client ID this token was issued to.
    pub client_id: String,
    /// Approved scopes.
    pub scopes: Vec<String>,
    /// Exact RFC 8707 resource/audience binding, if the grant specified one.
    pub resource: Option<String>,
    /// When the token was issued.
    pub issued_at: Instant,
    /// When the token expires.
    pub expires_at: Instant,
    /// Subject (user) this token was issued for.
    pub subject: Option<String>,
    /// Whether this is a refresh token.
    pub is_refresh_token: bool,
}

impl std::fmt::Debug for OAuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthToken")
            .field("client_id_len", &self.client_id.len())
            .field("scope_count", &self.scopes.len())
            .field("resource_present", &self.resource.is_some())
            .field("subject_present", &self.subject.is_some())
            .finish_non_exhaustive()
    }
}

impl OAuthToken {
    /// Checks if this token has expired.
    #[must_use]
    pub fn is_expired(&self) -> bool {
        Instant::now() >= self.expires_at
    }

    /// Returns the remaining lifetime in seconds.
    #[must_use]
    pub fn expires_in_secs(&self) -> u64 {
        self.expires_at
            .saturating_duration_since(Instant::now())
            .as_secs()
    }
}

/// Server-retained token metadata augmented with its revocation family.
///
/// `OAuthToken` remains the secret-free public introspection model; the grant
/// identifier is an internal authorization-server correlation key.
#[derive(Clone)]
pub(crate) struct StoredOAuthToken {
    metadata: OAuthToken,
    grant_id: OAuthGrantId,
    registration_epoch: OAuthRegistrationEpoch,
    /// Absolute, non-sliding deadline for the complete refresh-token family.
    family_expires_at: Instant,
}

impl std::ops::Deref for StoredOAuthToken {
    type Target = OAuthToken;

    fn deref(&self) -> &Self::Target {
        &self.metadata
    }
}

/// Token response for successful token issuance.
///
/// The response deliberately is not `Clone`: it is the sole post-commit owner
/// of the raw access and refresh credentials returned to the caller.
#[derive(serde::Serialize)]
pub struct TokenResponse {
    /// The access token.
    pub access_token: String,
    /// Token type (always "bearer").
    pub token_type: String,
    /// Token lifetime in seconds.
    pub expires_in: u64,
    /// Refresh token (if issued).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Granted scopes (space-separated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

impl std::fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token_len", &self.access_token.len())
            .field("token_type_len", &self.token_type.len())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field(
                "scope_count",
                &self
                    .scope
                    .as_deref()
                    .map_or(0, |scope| scope.split_ascii_whitespace().count()),
            )
            .finish()
    }
}

// =============================================================================
// Authorization Request
// =============================================================================

/// Immutable generation of the authorization-approval policy installed in an
/// [`OAuthServer`].
///
/// A backend must return this exact generation in an approved decision. It is
/// deliberately an opaque value: callers can compare generations but cannot
/// inspect policy material through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthorizationApprovalGeneration([u8; 32]);

impl AuthorizationApprovalGeneration {
    /// Creates an opaque approval-policy generation from fixed-width bytes.
    #[must_use]
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

#[derive(Clone, PartialEq, Eq)]
struct AuthorizationApprovalBinding {
    client_id: String,
    redirect_uri: String,
    scopes: Vec<String>,
    resource: Option<String>,
    state: Option<String>,
    code_challenge: String,
    code_challenge_method: CodeChallengeMethod,
    registration_epoch: OAuthRegistrationEpoch,
}

/// Redacted, immutable request presented to the authorization/consent
/// backend after OAuth request and client validation succeeds.
///
/// It never contains a client secret, authorization code, access token, or
/// refresh token. The fields are bounded and canonicalized before this value
/// is created.
pub struct AuthorizationApprovalRequest {
    binding: AuthorizationApprovalBinding,
}

impl AuthorizationApprovalRequest {
    /// Validated client identifier.
    #[must_use]
    pub fn client_id(&self) -> &str {
        &self.binding.client_id
    }

    /// Validated redirect URI.
    #[must_use]
    pub fn redirect_uri(&self) -> &str {
        &self.binding.redirect_uri
    }

    /// Canonical requested scopes.
    #[must_use]
    pub fn scopes(&self) -> &[String] {
        &self.binding.scopes
    }

    /// Validated RFC 8707 resource indicator, if one was requested.
    #[must_use]
    pub fn resource(&self) -> Option<&str> {
        self.binding.resource.as_deref()
    }

    /// Caller-supplied state, if present.
    #[must_use]
    pub fn state(&self) -> Option<&str> {
        self.binding.state.as_deref()
    }

    /// S256 PKCE challenge identifier.
    #[must_use]
    pub fn code_challenge(&self) -> &str {
        &self.binding.code_challenge
    }

    /// Validated PKCE method (always S256 on this server).
    #[must_use]
    pub const fn code_challenge_method(&self) -> CodeChallengeMethod {
        self.binding.code_challenge_method
    }

    /// Produces a non-forgeable approval decision bound to this exact request.
    ///
    /// The decision is intentionally neither cloneable nor serializable. A
    /// backend may approve only the exact canonical scopes and resource it was
    /// shown; any mismatch is rejected by [`OAuthServer::authorize`] before a
    /// code is drawn or state is changed.
    pub fn approve(
        &self,
        subject: String,
        approved_scopes: Vec<String>,
        approved_resource: Option<String>,
        generation: AuthorizationApprovalGeneration,
    ) -> Result<AuthorizationApprovalDecision, OAuthError> {
        validate_authorization_subject(&subject)?;
        Ok(AuthorizationApprovalDecision {
            binding: self.binding.clone(),
            subject,
            approved_scopes,
            approved_resource,
            generation,
        })
    }
}

impl std::fmt::Debug for AuthorizationApprovalRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationApprovalRequest")
            .field("client_id_len", &self.binding.client_id.len())
            .field("redirect_uri_len", &self.binding.redirect_uri.len())
            .field("scope_count", &self.binding.scopes.len())
            .field("resource_present", &self.binding.resource.is_some())
            .field("state_present", &self.binding.state.is_some())
            .field("code_challenge_len", &self.binding.code_challenge.len())
            .finish_non_exhaustive()
    }
}

/// One-shot approval decision produced only by an
/// [`AuthorizationApprovalBackend`].
///
/// Its fields are private and it deliberately implements neither `Clone` nor
/// serialization, preventing reuse or network transport of an approval
/// receipt.
pub struct AuthorizationApprovalDecision {
    binding: AuthorizationApprovalBinding,
    subject: String,
    approved_scopes: Vec<String>,
    approved_resource: Option<String>,
    generation: AuthorizationApprovalGeneration,
}

impl std::fmt::Debug for AuthorizationApprovalDecision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationApprovalDecision")
            .field("subject_len", &self.subject.len())
            .field("scope_count", &self.approved_scopes.len())
            .field("resource_present", &self.approved_resource.is_some())
            .finish_non_exhaustive()
    }
}

/// Result of a synchronous authorization/consent backend invocation.
#[allow(
    clippy::large_enum_variant,
    reason = "the decision is a deliberate one-shot, non-cloneable approval capability payload"
)]
pub enum AuthorizationApprovalDisposition {
    /// The backend approved the exact redacted request.
    Approved(AuthorizationApprovalDecision),
    /// The resource owner denied the request.
    Denied,
    /// The backend could not reach a decision.
    Error,
    /// The interaction was cancelled before approval completed.
    Cancelled,
}

impl std::fmt::Debug for AuthorizationApprovalDisposition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Approved(_) => f.write_str("AuthorizationApprovalDisposition::Approved(..)"),
            Self::Denied => f.write_str("AuthorizationApprovalDisposition::Denied"),
            Self::Error => f.write_str("AuthorizationApprovalDisposition::Error"),
            Self::Cancelled => f.write_str("AuthorizationApprovalDisposition::Cancelled"),
        }
    }
}

/// Synchronous, sealed authorization/consent policy boundary.
///
/// This API is intentionally synchronous because the existing OAuth server is
/// synchronous. An asynchronous backend would require a cancellation-correct
/// `Cx` threading change and is not represented by this surface.
pub trait AuthorizationApprovalBackend: Send + Sync {
    /// Immutable generation of the installed backend configuration.
    fn generation(&self) -> AuthorizationApprovalGeneration;

    /// Decides one already-validated authorization request.
    fn approve(&self, request: &AuthorizationApprovalRequest) -> AuthorizationApprovalDisposition;
}

struct DenyAllAuthorizationApprovalBackend;

impl AuthorizationApprovalBackend for DenyAllAuthorizationApprovalBackend {
    fn generation(&self) -> AuthorizationApprovalGeneration {
        AuthorizationApprovalGeneration::from_bytes([0; 32])
    }

    fn approve(&self, _request: &AuthorizationApprovalRequest) -> AuthorizationApprovalDisposition {
        AuthorizationApprovalDisposition::Denied
    }
}

#[cfg(test)]
struct TestDefaultAuthorizationApprovalBackend;

#[cfg(test)]
impl AuthorizationApprovalBackend for TestDefaultAuthorizationApprovalBackend {
    fn generation(&self) -> AuthorizationApprovalGeneration {
        AuthorizationApprovalGeneration::from_bytes([0x54; 32])
    }

    fn approve(&self, request: &AuthorizationApprovalRequest) -> AuthorizationApprovalDisposition {
        AuthorizationApprovalDisposition::Approved(
            request
                .approve(
                    "oauth-test-subject".to_string(),
                    request.scopes().to_vec(),
                    request.resource().map(str::to_string),
                    self.generation(),
                )
                .expect("validated test approval request must produce a decision"),
        )
    }
}

/// Authorization request parameters.
#[derive(Clone)]
pub struct AuthorizationRequest {
    /// Response type (must be "code" for authorization code flow).
    pub response_type: String,
    /// Client ID.
    pub client_id: String,
    /// Redirect URI.
    pub redirect_uri: String,
    /// Requested scopes (space-separated in original request).
    pub scopes: Vec<String>,
    /// RFC 8707 resource indicator requested for this authorization.
    pub resource: Option<String>,
    /// State parameter (recommended for CSRF protection).
    pub state: Option<String>,
    /// PKCE code challenge.
    pub code_challenge: String,
    /// PKCE code challenge method.
    pub code_challenge_method: CodeChallengeMethod,
}

impl std::fmt::Debug for AuthorizationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthorizationRequest")
            .field("response_type_len", &self.response_type.len())
            .field("client_id_len", &self.client_id.len())
            .field("redirect_uri_len", &self.redirect_uri.len())
            .field("scope_count", &self.scopes.len())
            .field("resource_present", &self.resource.is_some())
            .field("state_present", &self.state.is_some())
            .field("code_challenge_len", &self.code_challenge.len())
            .finish()
    }
}

/// Token request parameters.
pub struct TokenRequest {
    /// Grant type.
    pub grant_type: String,
    /// Authorization code (for authorization_code grant).
    pub code: Option<String>,
    /// Redirect URI (for authorization_code grant).
    pub redirect_uri: Option<String>,
    /// Client ID.
    pub client_id: String,
    /// Client secret (for confidential clients).
    pub client_secret: Option<String>,
    /// PKCE code verifier.
    pub code_verifier: Option<String>,
    /// Refresh token (for refresh_token grant).
    pub refresh_token: Option<String>,
    /// Requested scopes (for refresh_token grant, subset of original scopes).
    pub scopes: Option<Vec<String>>,
    /// RFC 8707 resource indicator. It must exactly match the bound
    /// authorization-code or refresh-token resource when supplied.
    pub resource: Option<String>,
}

impl std::fmt::Debug for TokenRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenRequest")
            .field("grant_type_len", &self.grant_type.len())
            .field("code_present", &self.code.is_some())
            .field("redirect_uri_present", &self.redirect_uri.is_some())
            .field("client_id_len", &self.client_id.len())
            .field("client_secret_present", &self.client_secret.is_some())
            .field("code_verifier_present", &self.code_verifier.is_some())
            .field("refresh_token_present", &self.refresh_token.is_some())
            .field("scope_count", &self.scopes.as_ref().map_or(0, Vec::len))
            .field("resource_present", &self.resource.is_some())
            .finish()
    }
}

// =============================================================================
// OAuth Errors
// =============================================================================

/// OAuth error types following RFC 6749.
#[derive(Clone)]
pub enum OAuthError {
    /// The request is missing a required parameter or is otherwise malformed.
    InvalidRequest(String),
    /// Client authentication failed.
    InvalidClient(String),
    /// The authorization grant or refresh token is invalid.
    InvalidGrant(String),
    /// The client is not authorized to use this grant type.
    UnauthorizedClient(String),
    /// The grant type is not supported.
    UnsupportedGrantType(String),
    /// The requested scope is invalid or unknown.
    InvalidScope(String),
    /// The authorization server encountered an unexpected condition.
    ServerError(String),
    /// The authorization server is temporarily unavailable.
    TemporarilyUnavailable(String),
    /// Access denied by the resource owner.
    AccessDenied(String),
    /// The response type is not supported.
    UnsupportedResponseType(String),
}

impl std::fmt::Debug for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (variant, description) = match self {
            Self::InvalidRequest(description) => ("InvalidRequest", description),
            Self::InvalidClient(description) => ("InvalidClient", description),
            Self::InvalidGrant(description) => ("InvalidGrant", description),
            Self::UnauthorizedClient(description) => ("UnauthorizedClient", description),
            Self::UnsupportedGrantType(description) => ("UnsupportedGrantType", description),
            Self::InvalidScope(description) => ("InvalidScope", description),
            Self::ServerError(description) => ("ServerError", description),
            Self::TemporarilyUnavailable(description) => ("TemporarilyUnavailable", description),
            Self::AccessDenied(description) => ("AccessDenied", description),
            Self::UnsupportedResponseType(description) => ("UnsupportedResponseType", description),
        };

        f.debug_struct(variant)
            .field("description_len", &description.len())
            .finish()
    }
}

impl OAuthError {
    /// Returns the OAuth error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::InvalidClient(_) => "invalid_client",
            Self::InvalidGrant(_) => "invalid_grant",
            Self::UnauthorizedClient(_) => "unauthorized_client",
            Self::UnsupportedGrantType(_) => "unsupported_grant_type",
            Self::InvalidScope(_) => "invalid_scope",
            Self::ServerError(_) => "server_error",
            Self::TemporarilyUnavailable(_) => "temporarily_unavailable",
            Self::AccessDenied(_) => "access_denied",
            Self::UnsupportedResponseType(_) => "unsupported_response_type",
        }
    }

    /// Returns the error description.
    #[must_use]
    pub fn description(&self) -> &str {
        match self {
            Self::InvalidRequest(s)
            | Self::InvalidClient(s)
            | Self::InvalidGrant(s)
            | Self::UnauthorizedClient(s)
            | Self::UnsupportedGrantType(s)
            | Self::InvalidScope(s)
            | Self::ServerError(s)
            | Self::TemporarilyUnavailable(s)
            | Self::AccessDenied(s)
            | Self::UnsupportedResponseType(s) => s,
        }
    }
}

impl std::fmt::Display for OAuthError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.error_code(), self.description())
    }
}

impl std::error::Error for OAuthError {}

impl From<OAuthError> for McpError {
    fn from(err: OAuthError) -> Self {
        match &err {
            OAuthError::InvalidClient(_) | OAuthError::UnauthorizedClient(_) => {
                McpError::new(McpErrorCode::ResourceForbidden, err.to_string())
            }
            OAuthError::AccessDenied(_) => {
                McpError::new(McpErrorCode::ResourceForbidden, err.to_string())
            }
            _ => McpError::new(McpErrorCode::InvalidRequest, err.to_string()),
        }
    }
}

// =============================================================================
// OAuth Server
// =============================================================================

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct CredentialDigest(Sha256Digest);

impl std::fmt::Debug for CredentialDigest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CredentialDigest([redacted; 32 bytes])")
    }
}

#[derive(Clone, Copy)]
enum CredentialKind {
    AuthorizationCode,
    AccessToken,
    RefreshToken,
}

impl CredentialKind {
    fn domain(self) -> &'static [u8] {
        match self {
            Self::AuthorizationCode => AUTHORIZATION_CODE_DIGEST_DOMAIN,
            Self::AccessToken => ACCESS_TOKEN_DIGEST_DOMAIN,
            Self::RefreshToken => REFRESH_TOKEN_DIGEST_DOMAIN,
        }
    }
}

/// Expiry-carrying record for a revoked or rotated token.
#[derive(Debug, Clone)]
pub(crate) struct RevocationTombstone {
    /// Client that owned the removed credential.
    pub(crate) client_id: String,
    /// Grant family that owned the removed credential.
    pub(crate) grant_id: OAuthGrantId,
    /// Whether this marker must be retained while the refresh chain remains
    /// live so reuse can trigger family-wide invalidation.
    pub(crate) replay_guard: bool,
    /// Retention deadline. Ordinary revocations use the credential expiry;
    /// live-chain replay guards use the family's fixed absolute deadline.
    pub(crate) expires_at: Instant,
}

/// Internal state for the OAuth server.
pub(crate) struct OAuthServerState {
    /// Registered clients by client_id.
    clients: HashMap<String, RegisteredOAuthClient>,
    /// Pending authorization codes.
    pub(crate) authorization_codes: HashMap<CredentialDigest, AuthorizationCode>,
    /// Active access tokens.
    pub(crate) access_tokens: HashMap<CredentialDigest, StoredOAuthToken>,
    /// Active refresh tokens.
    pub(crate) refresh_tokens: HashMap<CredentialDigest, StoredOAuthToken>,
    /// Revoked tokens retained to their own expiry; refresh replay guards are
    /// retained to the fixed absolute deadline of their grant family.
    pub(crate) revoked_tokens: HashMap<CredentialDigest, RevocationTombstone>,
}

impl OAuthServerState {
    fn new() -> Self {
        Self {
            clients: HashMap::new(),
            authorization_codes: HashMap::new(),
            access_tokens: HashMap::new(),
            refresh_tokens: HashMap::new(),
            revoked_tokens: HashMap::new(),
        }
    }

    fn cleanup_expired_at(&mut self, now: Instant) {
        self.authorization_codes
            .retain(|_, code| code.expires_at > now);
        self.access_tokens.retain(|_, token| token.expires_at > now);
        self.refresh_tokens
            .retain(|_, token| token.expires_at > now);
        self.revoked_tokens
            .retain(|_, tombstone| tombstone.expires_at > now);
    }

    fn authorization_code_count_for_client(&self, client_id: &str) -> usize {
        self.authorization_codes
            .values()
            .filter(|code| code.client_id == client_id)
            .count()
    }

    fn access_token_count_for_client(&self, client_id: &str) -> usize {
        self.access_tokens
            .values()
            .filter(|token| token.client_id == client_id)
            .count()
    }

    fn refresh_token_count_for_client(&self, client_id: &str) -> usize {
        self.refresh_tokens
            .values()
            .filter(|token| token.client_id == client_id)
            .count()
    }

    fn revocation_tombstone_count_for_client(&self, client_id: &str) -> usize {
        self.revoked_tokens
            .values()
            .filter(|tombstone| tombstone.client_id == client_id)
            .count()
    }

    fn credential_value_in_use(&self, value: &str) -> bool {
        let authorization_code = digest_credential(CredentialKind::AuthorizationCode, value);
        let access_token = digest_credential(CredentialKind::AccessToken, value);
        let refresh_token = digest_credential(CredentialKind::RefreshToken, value);

        authorization_code.is_ok_and(|digest| self.authorization_codes.contains_key(&digest))
            || access_token.is_ok_and(|digest| {
                self.access_tokens.contains_key(&digest)
                    || self.revoked_tokens.contains_key(&digest)
            })
            || refresh_token.is_ok_and(|digest| {
                self.refresh_tokens.contains_key(&digest)
                    || self.revoked_tokens.contains_key(&digest)
            })
    }

    fn insert_tombstone_bounded(
        &mut self,
        token: CredentialDigest,
        tombstone: RevocationTombstone,
        config: &OAuthServerConfig,
        now: Instant,
    ) {
        if tombstone.expires_at <= now
            || config.max_revocation_tombstones == 0
            || config.max_revocation_tombstones_per_client == 0
        {
            return;
        }

        if let Some(existing) = self.revoked_tokens.get_mut(&token) {
            existing.replay_guard |= tombstone.replay_guard;
            if tombstone.expires_at > existing.expires_at {
                existing.expires_at = tombstone.expires_at;
            }
            return;
        }

        if self.revocation_tombstone_count_for_client(&tombstone.client_id)
            >= config.max_revocation_tombstones_per_client
        {
            let oldest_for_client = self
                .revoked_tokens
                .iter()
                .filter(|(_, entry)| entry.client_id == tombstone.client_id && !entry.replay_guard)
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(value, _)| *value);
            if let Some(oldest) = oldest_for_client {
                self.revoked_tokens.remove(&oldest);
            } else {
                return;
            }
        }

        if self.revoked_tokens.len() >= config.max_revocation_tombstones {
            let oldest = self
                .revoked_tokens
                .iter()
                .filter(|(_, entry)| !entry.replay_guard)
                .min_by_key(|(_, entry)| entry.expires_at)
                .map(|(value, _)| *value);
            if let Some(oldest) = oldest {
                self.revoked_tokens.remove(&oldest);
            } else {
                return;
            }
        }

        self.revoked_tokens.insert(token, tombstone);
    }

    fn ensure_replay_guard_capacity(
        &self,
        client_id: &str,
        config: &OAuthServerConfig,
    ) -> Result<(), OAuthError> {
        let client_count = self.revocation_tombstone_count_for_client(client_id);
        if client_count >= config.max_revocation_tombstones_per_client
            && !self
                .revoked_tokens
                .values()
                .any(|entry| entry.client_id == client_id && !entry.replay_guard)
        {
            return Err(capacity_error("refresh-token replay guards"));
        }
        if self.revoked_tokens.len() >= config.max_revocation_tombstones
            && !self
                .revoked_tokens
                .values()
                .any(|entry| !entry.replay_guard)
        {
            return Err(capacity_error("refresh-token replay guards"));
        }
        Ok(())
    }

    fn align_replay_guards_with_family_deadline(
        &mut self,
        grant_id: OAuthGrantId,
        client_id: &str,
        family_expires_at: Instant,
    ) {
        for tombstone in self.revoked_tokens.values_mut().filter(|entry| {
            entry.replay_guard && entry.client_id == client_id && entry.grant_id == grant_id
        }) {
            tombstone.expires_at = family_expires_at;
        }
    }

    /// Atomically removes every active token descended from `grant_id` and
    /// leaves bounded, expiry-carrying tombstones for the removed credentials.
    fn revoke_grant_family(
        &mut self,
        grant_id: OAuthGrantId,
        client_id: &str,
        config: &OAuthServerConfig,
        now: Instant,
    ) {
        let access_tokens: Vec<_> = self
            .access_tokens
            .iter()
            .filter(|(_, token)| token.client_id == client_id && token.grant_id == grant_id)
            .map(|(digest, token)| (*digest, token.expires_at))
            .collect();
        let refresh_tokens: Vec<_> = self
            .refresh_tokens
            .iter()
            .filter(|(_, token)| token.client_id == client_id && token.grant_id == grant_id)
            .map(|(digest, token)| (*digest, token.expires_at))
            .collect();

        // Once the complete removal set has been prepared, its replay guards
        // can release bounded tombstone capacity before terminal markers are
        // inserted. All validation and removal-set collection has completed.
        self.revoked_tokens.retain(|_, tombstone| {
            !(tombstone.replay_guard
                && tombstone.client_id == client_id
                && tombstone.grant_id == grant_id)
        });

        for (digest, expires_at) in access_tokens {
            self.access_tokens.remove(&digest);
            self.insert_tombstone_bounded(
                digest,
                RevocationTombstone {
                    client_id: client_id.to_string(),
                    grant_id,
                    replay_guard: false,
                    expires_at,
                },
                config,
                now,
            );
        }
        for (digest, expires_at) in refresh_tokens {
            self.refresh_tokens.remove(&digest);
            self.insert_tombstone_bounded(
                digest,
                RevocationTombstone {
                    client_id: client_id.to_string(),
                    grant_id,
                    replay_guard: false,
                    expires_at,
                },
                config,
                now,
            );
        }
    }
}

struct PreparedTokenPair {
    access_value: String,
    access_digest: CredentialDigest,
    access_metadata: StoredOAuthToken,
    refresh_value: String,
    refresh_digest: CredentialDigest,
    refresh_metadata: StoredOAuthToken,
    access_lifetime_secs: u64,
}

impl PreparedTokenPair {
    fn issued_at(&self) -> Instant {
        self.access_metadata.issued_at
    }

    fn into_response_and_records(
        self,
    ) -> (
        TokenResponse,
        (CredentialDigest, StoredOAuthToken),
        (CredentialDigest, StoredOAuthToken),
    ) {
        let response = TokenResponse {
            access_token: self.access_value,
            token_type: self.access_metadata.token_type.as_str().to_string(),
            expires_in: self.access_lifetime_secs,
            refresh_token: Some(self.refresh_value),
            scope: if self.access_metadata.scopes.is_empty() {
                None
            } else {
                Some(self.access_metadata.scopes.join(" "))
            },
        };
        (
            response,
            (self.access_digest, self.access_metadata),
            (self.refresh_digest, self.refresh_metadata),
        )
    }
}

/// OAuth 2.0/2.1 authorization server.
///
/// This server contains an OAuth authorization-code path that requires PKCE.
/// That implementation policy is not an OAuth profile-conformance claim.
pub struct OAuthServer {
    config: OAuthServerConfig,
    approval_backend: Arc<dyn AuthorizationApprovalBackend>,
    approval_generation: AuthorizationApprovalGeneration,
    pub(crate) state: RwLock<OAuthServerState>,
}

impl OAuthServer {
    /// Creates a new OAuth server with the given configuration.
    ///
    /// Invalid configurations remain fail-closed: mutation methods validate
    /// the configuration before changing state. Use [`Self::try_new`] to
    /// reject invalid configuration eagerly.
    #[must_use]
    pub fn new(config: OAuthServerConfig) -> Self {
        Self::with_approval_backend(config, Arc::new(DenyAllAuthorizationApprovalBackend))
    }

    /// Creates an OAuth server with an installed authorization/consent backend.
    ///
    /// The backend is called exactly once after request and client validation,
    /// and before credential generation or state mutation. The default
    /// [`Self::new`] constructor installs a fail-closed deny-all backend.
    #[must_use]
    pub fn with_approval_backend(
        config: OAuthServerConfig,
        approval_backend: Arc<dyn AuthorizationApprovalBackend>,
    ) -> Self {
        let approval_generation = approval_backend.generation();
        Self {
            config,
            approval_backend,
            approval_generation,
            state: RwLock::new(OAuthServerState::new()),
        }
    }

    /// Creates a new OAuth server after validating its configuration.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid PKCE bounds, state caps outside the hard
    /// per-field or aggregate retention envelope, incoherent state caps, or
    /// unsafe token lifetimes.
    pub fn try_new(config: OAuthServerConfig) -> Result<Self, OAuthError> {
        config.validate()?;
        Ok(Self::new(config))
    }

    /// Creates a validated OAuth server with an installed approval backend.
    pub fn try_with_approval_backend(
        config: OAuthServerConfig,
        approval_backend: Arc<dyn AuthorizationApprovalBackend>,
    ) -> Result<Self, OAuthError> {
        config.validate()?;
        Ok(Self::with_approval_backend(config, approval_backend))
    }

    /// Creates a new OAuth server with default configuration.
    #[must_use]
    pub fn with_defaults() -> Self {
        #[cfg(test)]
        {
            Self::with_approval_backend(
                OAuthServerConfig::default(),
                Arc::new(TestDefaultAuthorizationApprovalBackend),
            )
        }
        #[cfg(not(test))]
        {
            Self::new(OAuthServerConfig::default())
        }
    }

    /// Returns the server configuration.
    #[must_use]
    pub fn config(&self) -> &OAuthServerConfig {
        &self.config
    }

    fn state_for_mutation(
        &self,
    ) -> Result<(std::sync::RwLockWriteGuard<'_, OAuthServerState>, Instant), OAuthError> {
        self.config.validate()?;
        let mut state = self
            .state
            .write()
            .map_err(|_| OAuthError::ServerError("failed to acquire write lock".to_string()))?;
        // Capture time only after acquiring the write lock. A caller may have
        // waited arbitrarily long behind another mutation.
        let now = Instant::now();
        state.cleanup_expired_at(now);
        Ok((state, now))
    }

    /// Acquires the mutation gate only after authenticating the client while
    /// holding the same write lock that will commit the transition. Failed
    /// client authentication must not perform opportunistic expiry cleanup:
    /// an otherwise denied request is not allowed to mutate retained OAuth
    /// state before it is rejected.
    fn state_for_authenticated_mutation(
        &self,
        client_id: &str,
        client_secret: Option<&str>,
    ) -> Result<
        (
            std::sync::RwLockWriteGuard<'_, OAuthServerState>,
            Instant,
            OAuthRegistrationEpoch,
        ),
        OAuthError,
    > {
        self.config.validate()?;
        let mut state = self
            .state
            .write()
            .map_err(|_| OAuthError::ServerError("failed to acquire write lock".to_string()))?;
        let registration_epoch = authenticate_client_or_dummy(&state, client_id, client_secret)?;
        let now = Instant::now();
        state.cleanup_expired_at(now);
        Ok((state, now, registration_epoch))
    }

    /// Authenticates the client before expiry cleanup can occur. Resource
    /// mismatches remain strict no-ops and preserve their existing
    /// pre-authentication ordering.
    fn state_for_authenticated_resource_checked_mutation<F>(
        &self,
        client_id: &str,
        client_secret: Option<&str>,
        resource_matches: F,
    ) -> Result<
        (
            std::sync::RwLockWriteGuard<'_, OAuthServerState>,
            Instant,
            OAuthRegistrationEpoch,
        ),
        OAuthError,
    >
    where
        F: FnOnce(&OAuthServerState) -> bool,
    {
        self.config.validate()?;
        let mut state = self
            .state
            .write()
            .map_err(|_| OAuthError::ServerError("failed to acquire write lock".to_string()))?;
        if !resource_matches(&state) {
            return Err(invalid_grant_error());
        }
        let registration_epoch = authenticate_client_or_dummy(&state, client_id, client_secret)?;
        let now = Instant::now();
        state.cleanup_expired_at(now);
        Ok((state, now, registration_epoch))
    }

    // -------------------------------------------------------------------------
    // Client Registration
    // -------------------------------------------------------------------------

    /// Registers a new OAuth client.
    ///
    /// # Errors
    ///
    /// Returns an error if:
    /// - Public client fields were mutated into an inconsistent shape
    /// - Client metadata exceeds a retained-value or retained-count bound
    /// - A client with the same ID already exists
    /// - Public clients are not allowed and the client has no secret
    /// - The configured client capacity has been reached
    pub fn register_client(&self, client: OAuthClient) -> Result<(), OAuthError> {
        // OAuthClient fields are public for API ergonomics, so builder-time
        // validation is not a retention boundary. Revalidate the complete
        // object immediately before it can enter persistent server state.
        client.validate_for_retention()?;

        if client.client_type == ClientType::Public && !self.config.allow_public_clients {
            return Err(OAuthError::InvalidClient(
                "public clients are not allowed".to_string(),
            ));
        }

        let registration_epoch = OAuthRegistrationEpoch::draw()?;
        let client = RegisteredOAuthClient::from_registration(client, registration_epoch)?;
        let client_id = client.metadata.client_id.clone();

        let (mut state, _) = self.state_for_mutation()?;

        if state.clients.contains_key(&client_id) {
            return Err(OAuthError::InvalidClient(
                "OAuth client_id is already registered".to_string(),
            ));
        }

        if state.clients.len() >= self.config.max_clients {
            return Err(capacity_error("registered clients"));
        }

        state.clients.insert(client_id, client);
        Ok(())
    }

    /// Unregisters an OAuth client.
    ///
    /// This also revokes all tokens issued to the client.
    pub fn unregister_client(&self, client_id: &str) -> Result<(), OAuthError> {
        validate_client_id_admission(client_id)?;
        let (mut state, _) = self.state_for_mutation()?;

        if !state.clients.contains_key(client_id) {
            return Err(OAuthError::InvalidClient(
                OAUTH_CLIENT_NOT_FOUND_ERROR.to_string(),
            ));
        }

        state.clients.remove(client_id);
        state
            .authorization_codes
            .retain(|_, code| code.client_id != client_id);
        state
            .access_tokens
            .retain(|_, token| token.client_id != client_id);
        state
            .refresh_tokens
            .retain(|_, token| token.client_id != client_id);
        // No descendant remains after unregistration. Purging the old
        // registration's tombstones prevents a later registration with the
        // same public client ID from inheriting replay-guard capacity or state.
        state
            .revoked_tokens
            .retain(|_, tombstone| tombstone.client_id != client_id);

        Ok(())
    }

    /// Gets secret-free metadata for a registered client by ID.
    ///
    /// Confidential client credentials are never cloned into this
    /// administrative read model.
    #[must_use]
    pub fn get_client(&self, client_id: &str) -> Option<OAuthClientMetadata> {
        if validate_client_id_admission(client_id).is_err() {
            return None;
        }
        self.state
            .read()
            .ok()
            .and_then(|s| s.clients.get(client_id).map(OAuthClientMetadata::from))
    }

    /// Lists secret-free metadata for all registered clients.
    ///
    /// Confidential client credentials are never cloned into this
    /// administrative read model.
    #[must_use]
    pub fn list_clients(&self) -> Vec<OAuthClientMetadata> {
        let mut clients: Vec<OAuthClientMetadata> = self
            .state
            .read()
            .map(|s| s.clients.values().map(OAuthClientMetadata::from).collect())
            .unwrap_or_default();
        clients.sort_unstable_by(|left, right| left.client_id.cmp(&right.client_id));
        clients
    }

    /// Registers one bounded RFC 8252 native public client from the HTTP DCR
    /// profile. Client IDs are framework-generated CSPRNG values and no secret
    /// or registration-management credential is created.
    pub(crate) fn register_native_public_client(
        &self,
        request: NativePublicClientRegistrationRequest,
    ) -> Result<NativePublicClientRegistrationResponse, OAuthError> {
        self.config.validate()?;
        if !self.config.allow_public_clients {
            return Err(OAuthError::InvalidClient(
                "public client registration is disabled".to_string(),
            ));
        }
        if request.application_type != "native"
            || request.token_endpoint_auth_method != "none"
            || request.response_types.len() != 1
            || request.response_types[0] != "code"
            || request.grant_types.is_empty()
            || request.grant_types.len() > 2
            || !request.grant_types.iter().any(|grant| grant == "authorization_code")
            || request.grant_types.iter().any(|grant| {
                grant != "authorization_code" && grant != "refresh_token"
            })
            || request.grant_types.iter().enumerate().any(|(index, grant)| {
                request.grant_types[..index].contains(grant)
            })
            || request.redirect_uris.iter().enumerate().any(|(index, uri)| {
                request.redirect_uris[..index].contains(uri)
            })
        {
            return Err(OAuthError::InvalidRequest(
                "dynamic registration is outside the native public-client profile".to_string(),
            ));
        }

        let scopes = match request.scope.as_deref() {
            None => Vec::new(),
            Some("") => {
                return Err(OAuthError::InvalidRequest(
                    "dynamic registration scope is empty".to_string(),
                ));
            }
            Some(scope) => {
                let values: Vec<String> = scope.split(' ').map(str::to_owned).collect();
                if values.iter().any(String::is_empty)
                    || values.iter().enumerate().any(|(index, value)| {
                        values[..index].contains(value)
                    })
                {
                    return Err(OAuthError::InvalidRequest(
                        "dynamic registration scope is not canonical".to_string(),
                    ));
                }
                values
            }
        };

        let client_id = generate_token()?;
        let client = OAuthClient::builder(client_id.clone())
            .redirect_uris(request.redirect_uris.clone())
            .scopes(scopes.clone())
            .name(request.client_name.clone())
            .build()?;
        self.register_client(client)?;

        Ok(NativePublicClientRegistrationResponse {
            client_id,
            client_name: request.client_name,
            application_type: "native",
            redirect_uris: request.redirect_uris,
            token_endpoint_auth_method: "none",
            grant_types: request.grant_types,
            response_types: request.response_types,
            scope: (!scopes.is_empty()).then(|| scopes.join(" ")),
        })
    }

    // -------------------------------------------------------------------------
    // Authorization Endpoint
    // -------------------------------------------------------------------------

    /// Validates an authorization request, obtains one backend approval, and
    /// creates an authorization code only for a matching approved decision.
    ///
    /// # Returns
    ///
    /// Returns the authorization code and redirect URI on success.
    pub fn authorize(
        &self,
        request: &AuthorizationRequest,
    ) -> Result<(String, String), OAuthError> {
        self.authorize_with_token_draw(request, draw_security_identifier)
    }

    /// Builds a safe authorization-error redirect after an authorization
    /// request failed.
    ///
    /// RFC 6749 permits a direct error only when the client or redirect URI
    /// cannot be trusted. This helper therefore re-validates exactly those
    /// two routing inputs without mutating OAuth state. A caller must use the
    /// returned URI only for an authorization-endpoint error response.
    pub(crate) fn authorization_error_redirect(
        &self,
        request: &AuthorizationRequest,
        error: &OAuthError,
    ) -> Option<String> {
        if matches!(error, OAuthError::InvalidClient(_))
            || validate_client_id_admission(&request.client_id).is_err()
            || parse_redirect_uri(&request.redirect_uri).is_none()
            || validate_optional_authorization_value(
                request.state.as_deref(),
                MAX_OAUTH_STATE_BYTES,
                OAUTH_AUTHORIZATION_STATE_RETENTION_ERROR,
            )
            .is_err()
        {
            return None;
        }

        let state = self.state.read().ok()?;
        let client = state.clients.get(&request.client_id)?;
        if !client.validate_redirect_uri(&request.redirect_uri) {
            return None;
        }
        drop(state);

        let mut redirect = request.redirect_uri.clone();
        let separator = if redirect.contains('?') { '&' } else { '?' };
        redirect.push(separator);
        redirect.push_str("error=");
        redirect.push_str(error.error_code());
        if let Some(state) = &request.state {
            redirect.push_str("&state=");
            redirect.push_str(&url_encode(state));
        }
        // Match successful authorization responses: the issuer identifier is
        // part of the authorization response, including failures.
        redirect.push_str("&iss=");
        redirect.push_str(&url_encode(&self.config.issuer));
        Some(redirect)
    }

    fn authorize_with_token_draw<F, E>(
        &self,
        request: &AuthorizationRequest,
        draw: F,
    ) -> Result<(String, String), OAuthError>
    where
        F: FnOnce() -> Result<SecurityIdentifier, E>,
        E: std::fmt::Display,
    {
        validate_client_id_admission(&request.client_id)?;
        if parse_redirect_uri(&request.redirect_uri).is_none() {
            return Err(OAuthError::InvalidRequest(
                "invalid redirect_uri".to_string(),
            ));
        }
        validate_optional_authorization_value(
            request.state.as_deref(),
            MAX_OAUTH_STATE_BYTES,
            OAUTH_AUTHORIZATION_STATE_RETENTION_ERROR,
        )?;
        validate_optional_authorization_resource(request.resource.as_deref())?;
        let canonical_scopes = canonicalize_request_scopes(&request.scopes)?;

        // Validate response_type
        if request.response_type != "code" {
            return Err(OAuthError::UnsupportedResponseType(
                "only 'code' response_type is supported".to_string(),
            ));
        }

        // Snapshot the exact registration that received this authorization
        // decision. The client ID alone is reusable after unregistration.
        let (client, approved_registration_epoch) = {
            let state = self
                .state
                .read()
                .map_err(|_| OAuthError::ServerError("failed to acquire read lock".to_string()))?;
            let registered = state.clients.get(&request.client_id).ok_or_else(|| {
                OAuthError::InvalidClient(OAUTH_CLIENT_NOT_FOUND_ERROR.to_string())
            })?;
            (
                OAuthClientMetadata::from(registered),
                registered.registration_epoch,
            )
        };

        // Validate redirect URI
        if !client.validate_redirect_uri(&request.redirect_uri) {
            return Err(OAuthError::InvalidRequest(
                "invalid redirect_uri".to_string(),
            ));
        }

        // Validate scopes
        if !client.validate_scopes(&canonical_scopes) {
            return Err(OAuthError::InvalidScope(
                "requested scope not allowed".to_string(),
            ));
        }

        // OAuth 2.1 requires PKCE, and this server deliberately supports only
        // S256. Accepting `plain` would let an intercepted challenge be used as
        // the verifier and would silently downgrade the authorization flow.
        if request.code_challenge_method != CodeChallengeMethod::S256 {
            return Err(OAuthError::InvalidRequest(
                "code_challenge_method must be S256".to_string(),
            ));
        }
        validate_s256_code_challenge(&request.code_challenge)?;
        self.config.validate()?;

        let approval_request = AuthorizationApprovalRequest {
            binding: AuthorizationApprovalBinding {
                client_id: request.client_id.clone(),
                redirect_uri: request.redirect_uri.clone(),
                scopes: canonical_scopes.clone(),
                resource: request.resource.clone(),
                state: request.state.clone(),
                code_challenge: request.code_challenge.clone(),
                code_challenge_method: request.code_challenge_method,
                registration_epoch: approved_registration_epoch,
            },
        };
        let approval = match self.approval_backend.approve(&approval_request) {
            AuthorizationApprovalDisposition::Approved(approval) => approval,
            AuthorizationApprovalDisposition::Denied => {
                return Err(OAuthError::AccessDenied(
                    "authorization approval was denied".to_string(),
                ));
            }
            AuthorizationApprovalDisposition::Error => {
                return Err(OAuthError::TemporarilyUnavailable(
                    "authorization approval backend failed".to_string(),
                ));
            }
            AuthorizationApprovalDisposition::Cancelled => {
                return Err(OAuthError::AccessDenied(
                    "authorization approval was cancelled".to_string(),
                ));
            }
        };
        if approval.binding != approval_request.binding
            || approval.generation != self.approval_generation
            || approval.approved_scopes != canonical_scopes
            || approval.approved_resource != request.resource
        {
            return Err(OAuthError::AccessDenied(
                "authorization approval did not bind the admitted request".to_string(),
            ));
        }

        // The accepted decision is consumed here before the code draw. No
        // denial, backend error, cancellation, or binding mismatch reaches
        // either random generation or mutable OAuth state.
        let subject = approval.subject;

        // Generate authorization code
        let code_value = generate_token_with_draw(draw)?;
        let code_digest = digest_credential(CredentialKind::AuthorizationCode, &code_value)?;
        // Store the code
        {
            let (mut state, now) = self.state_for_mutation()?;
            let current_client = state.clients.get(&request.client_id).ok_or_else(|| {
                OAuthError::InvalidClient(OAUTH_CLIENT_NOT_FOUND_ERROR.to_string())
            })?;
            if current_client.registration_epoch != approved_registration_epoch {
                return Err(OAuthError::InvalidClient(
                    "OAuth client registration changed during authorization".to_string(),
                ));
            }
            if !current_client.validate_redirect_uri(&request.redirect_uri) {
                return Err(OAuthError::InvalidRequest(
                    "invalid redirect_uri".to_string(),
                ));
            }
            if !current_client.validate_scopes(&canonical_scopes) {
                return Err(OAuthError::InvalidScope(
                    "requested scope not allowed".to_string(),
                ));
            }
            ensure_capacity(
                state.authorization_codes.len(),
                state.authorization_code_count_for_client(&request.client_id),
                self.config.max_authorization_codes,
                self.config.max_authorization_codes_per_client,
                "authorization codes",
            )?;
            if state.credential_value_in_use(&code_value) {
                return Err(OAuthError::ServerError(
                    "generated OAuth credential collided with retained state".to_string(),
                ));
            }
            let expires_at = checked_deadline(
                now,
                self.config.authorization_code_lifetime,
                "authorization_code_lifetime",
            )?;
            let code = AuthorizationCode {
                client_id: request.client_id.clone(),
                redirect_uri: request.redirect_uri.clone(),
                scopes: canonical_scopes.clone(),
                resource: request.resource.clone(),
                code_challenge: request.code_challenge.clone(),
                code_challenge_method: request.code_challenge_method,
                issued_at: now,
                expires_at,
                subject: Some(subject),
                state: request.state.clone(),
                registration_epoch: approved_registration_epoch,
            };
            state.authorization_codes.insert(code_digest, code);
        }

        // Build redirect URI with code
        let mut redirect = request.redirect_uri.clone();
        let separator = if redirect.contains('?') { '&' } else { '?' };
        redirect.push(separator);
        redirect.push_str("code=");
        redirect.push_str(&url_encode(&code_value));
        if let Some(state) = &request.state {
            redirect.push_str("&state=");
            redirect.push_str(&url_encode(state));
        }
        // RFC 9207 binds the authorization response to the issuing server.
        // Registered redirect URIs containing their own `iss` parameter are
        // rejected, so this cannot create an ambiguous duplicate.
        redirect.push_str("&iss=");
        redirect.push_str(&url_encode(&self.config.issuer));

        Ok((code_value, redirect))
    }

    // -------------------------------------------------------------------------
    // Token Endpoint
    // -------------------------------------------------------------------------

    /// Exchanges an authorization code or refresh token for tokens.
    pub fn token(&self, request: &TokenRequest) -> Result<TokenResponse, OAuthError> {
        match request.grant_type.as_str() {
            "authorization_code" => self.token_authorization_code(request),
            "refresh_token" => self.token_refresh_token(request),
            _ => Err(OAuthError::UnsupportedGrantType(
                OAUTH_GRANT_TYPE_UNSUPPORTED_ERROR.to_string(),
            )),
        }
    }

    fn token_authorization_code(
        &self,
        request: &TokenRequest,
    ) -> Result<TokenResponse, OAuthError> {
        self.token_authorization_code_with_draw(request, draw_security_identifier)
    }

    fn token_authorization_code_with_draw<F, E>(
        &self,
        request: &TokenRequest,
        mut draw: F,
    ) -> Result<TokenResponse, OAuthError>
    where
        F: FnMut() -> Result<SecurityIdentifier, E>,
        E: std::fmt::Display,
    {
        // Validate required parameters
        let code_value = request
            .code
            .as_ref()
            .ok_or_else(|| OAuthError::InvalidRequest("code is required".to_string()))?;
        let redirect_uri = request
            .redirect_uri
            .as_ref()
            .ok_or_else(|| OAuthError::InvalidRequest("redirect_uri is required".to_string()))?;
        let code_verifier = request.code_verifier.as_ref().ok_or_else(|| {
            OAuthError::InvalidRequest("code_verifier is required (PKCE)".to_string())
        })?;

        validate_client_authentication_admission(
            &request.client_id,
            request.client_secret.as_deref(),
        )?;
        let code_digest = validate_and_digest_opaque_credential(
            CredentialKind::AuthorizationCode,
            code_value,
            OAUTH_INVALID_GRANT_ERROR,
        )?;
        if parse_redirect_uri(redirect_uri).is_none() {
            return Err(invalid_grant_error());
        }
        validate_optional_authorization_resource(request.resource.as_deref())
            .map_err(|_| invalid_grant_error())?;

        // Enforce the fixed RFC 7636 syntax and hard bounds before consuming
        // the one-use authorization code or performing SHA-256.
        validate_pkce_code_verifier(code_verifier).map_err(|_| invalid_grant_error())?;
        if code_verifier.len() < self.config.min_code_verifier_length
            || code_verifier.len() > self.config.max_code_verifier_length
        {
            return Err(invalid_grant_error());
        }

        // Reject a mismatched resource under a read-only snapshot before any
        // write-side cleanup can affect otherwise unrelated expired state.
        // A missing code remains deferred to the mutation gate so existing
        // indistinguishable invalid-grant handling and cleanup semantics hold.
        {
            let state = self
                .state
                .read()
                .map_err(|_| OAuthError::ServerError("failed to acquire read lock".to_string()))?;
            if state
                .authorization_codes
                .get(&code_digest)
                .is_some_and(|code| request.resource != code.resource)
            {
                return Err(invalid_grant_error());
            }
        }

        // Validation, capacity admission, credential generation, one-time code
        // consumption, and token insertion share one write-side critical
        // section. Failed validation, capacity checks, or either random draw
        // leaves the authorization code available for a legitimate retry.
        let (mut state, now, current_registration_epoch) = self
            .state_for_authenticated_resource_checked_mutation(
                &request.client_id,
                request.client_secret.as_deref(),
                |state| {
                    state
                        .authorization_codes
                        .get(&code_digest)
                        .is_none_or(|code| request.resource == code.resource)
                },
            )?;
        let auth_code = state
            .authorization_codes
            .get(&code_digest)
            .cloned()
            .ok_or_else(invalid_grant_error)?;

        if auth_code.expires_at <= now {
            return Err(invalid_grant_error());
        }
        if auth_code.client_id != request.client_id {
            return Err(invalid_grant_error());
        }
        if auth_code.registration_epoch != current_registration_epoch {
            return Err(invalid_grant_error());
        }
        if auth_code.redirect_uri != *redirect_uri {
            return Err(invalid_grant_error());
        }
        if request.resource != auth_code.resource {
            return Err(invalid_grant_error());
        }
        if auth_code.code_challenge_method != CodeChallengeMethod::S256
            || !auth_code.validate_code_verifier(code_verifier)
        {
            return Err(invalid_grant_error());
        }

        ensure_capacity(
            state.access_tokens.len(),
            state.access_token_count_for_client(&auth_code.client_id),
            self.config.max_access_tokens,
            self.config.max_access_tokens_per_client,
            "access tokens",
        )?;
        ensure_capacity(
            state.refresh_tokens.len(),
            state.refresh_token_count_for_client(&auth_code.client_id),
            self.config.max_refresh_tokens,
            self.config.max_refresh_tokens_per_client,
            "refresh tokens",
        )?;

        let prepared = self.prepare_token_pair_with_draw(
            &auth_code.client_id,
            &auth_code.scopes,
            auth_code.resource.as_deref(),
            auth_code.subject.as_deref(),
            current_registration_epoch,
            Some(derive_authorization_grant_id(code_digest)?),
            None,
            &mut draw,
        )?;
        if auth_code.expires_at <= prepared.issued_at() {
            return Err(invalid_grant_error());
        }
        ensure_fresh_token_pair(&state, &prepared)?;
        let (response, access, refresh) = prepared.into_response_and_records();

        state
            .authorization_codes
            .remove(&code_digest)
            .ok_or_else(invalid_grant_error)?;
        state.access_tokens.insert(access.0, access.1);
        state.refresh_tokens.insert(refresh.0, refresh.1);

        Ok(response)
    }

    fn token_refresh_token(&self, request: &TokenRequest) -> Result<TokenResponse, OAuthError> {
        self.token_refresh_token_with_draw(request, draw_security_identifier)
    }

    fn token_refresh_token_with_draw<F, E>(
        &self,
        request: &TokenRequest,
        mut draw: F,
    ) -> Result<TokenResponse, OAuthError>
    where
        F: FnMut() -> Result<SecurityIdentifier, E>,
        E: std::fmt::Display,
    {
        let refresh_value = request
            .refresh_token
            .as_ref()
            .ok_or_else(|| OAuthError::InvalidRequest("refresh_token is required".to_string()))?;
        validate_client_authentication_admission(
            &request.client_id,
            request.client_secret.as_deref(),
        )?;
        let refresh_digest = validate_and_digest_opaque_credential(
            CredentialKind::RefreshToken,
            refresh_value,
            OAUTH_INVALID_GRANT_ERROR,
        )?;
        validate_optional_authorization_resource(request.resource.as_deref())
            .map_err(|_| invalid_grant_error())?;
        // As for authorization-code exchange, resource mismatch must be a
        // no-op even when the state contains unrelated expired entries.
        // Absence is deliberately deferred: a retained replay marker needs
        // the existing write-side family-revocation behavior.
        {
            let state = self
                .state
                .read()
                .map_err(|_| OAuthError::ServerError("failed to acquire read lock".to_string()))?;
            if state
                .refresh_tokens
                .get(&refresh_digest)
                .is_some_and(|token| {
                    request.resource.is_some() && request.resource != token.resource
                })
            {
                return Err(invalid_grant_error());
            }
        }
        // Validation, rotation, tombstoning, and insertion are one atomic
        // mutation. A successful refresh consumes the presented token exactly
        // once. Replaying a retained rotated-token marker revokes every live
        // descendant before returning the indistinguishable grant error.
        let (mut state, now, current_registration_epoch) = self
            .state_for_authenticated_resource_checked_mutation(
                &request.client_id,
                request.client_secret.as_deref(),
                |state| {
                    state
                        .refresh_tokens
                        .get(&refresh_digest)
                        .is_none_or(|token| {
                            request.resource.is_none() || request.resource == token.resource
                        })
                },
            )?;
        if let Some(revoked) = state.revoked_tokens.get(&refresh_digest).cloned() {
            if revoked.client_id == request.client_id {
                state.revoke_grant_family(revoked.grant_id, &request.client_id, &self.config, now);
            }
            return Err(invalid_grant_error());
        }

        let stored_refresh = state
            .refresh_tokens
            .get(&refresh_digest)
            .cloned()
            .ok_or_else(invalid_grant_error)?;
        if stored_refresh.client_id != request.client_id
            || stored_refresh.registration_epoch != current_registration_epoch
            || stored_refresh.expires_at <= now
            || stored_refresh.family_expires_at <= now
        {
            return Err(invalid_grant_error());
        }
        if request.resource.is_some() && request.resource != stored_refresh.resource {
            return Err(invalid_grant_error());
        }

        let canonical_requested_scopes = request
            .scopes
            .as_deref()
            .map(canonicalize_request_scopes)
            .transpose()?;

        // Determine scopes (subset of original if specified)
        let scopes = if let Some(requested) = canonical_requested_scopes {
            // Validate that requested scopes are a subset of original
            for scope in &requested {
                if !stored_refresh.scopes.contains(scope) {
                    return Err(OAuthError::InvalidScope(
                        "requested scope was not in original grant".to_string(),
                    ));
                }
            }
            requested
        } else {
            stored_refresh.scopes.clone()
        };

        ensure_capacity(
            state.access_tokens.len(),
            state.access_token_count_for_client(&request.client_id),
            self.config.max_access_tokens,
            self.config.max_access_tokens_per_client,
            "access tokens",
        )?;
        state.ensure_replay_guard_capacity(&request.client_id, &self.config)?;

        let prepared = self.prepare_token_pair_with_draw(
            &request.client_id,
            &scopes,
            stored_refresh.resource.as_deref(),
            stored_refresh.subject.as_deref(),
            current_registration_epoch,
            Some(stored_refresh.grant_id),
            Some(stored_refresh.family_expires_at),
            &mut draw,
        )?;
        ensure_fresh_token_pair(&state, &prepared)?;
        let replacement_refresh_expiry = prepared.refresh_metadata.expires_at;
        let (response, access, refresh) = prepared.into_response_and_records();

        let consumed = state
            .refresh_tokens
            .remove(&refresh_digest)
            .ok_or_else(invalid_grant_error)?;
        state.align_replay_guards_with_family_deadline(
            consumed.grant_id,
            &request.client_id,
            replacement_refresh_expiry,
        );
        state.insert_tombstone_bounded(
            refresh_digest,
            RevocationTombstone {
                client_id: consumed.client_id.clone(),
                grant_id: consumed.grant_id,
                replay_guard: true,
                expires_at: replacement_refresh_expiry,
            },
            &self.config,
            now,
        );
        state.access_tokens.insert(access.0, access.1);
        state.refresh_tokens.insert(refresh.0, refresh.1);

        Ok(response)
    }

    fn issue_tokens(
        &self,
        client_id: &str,
        scopes: &[String],
        subject: Option<&str>,
    ) -> Result<TokenResponse, OAuthError> {
        self.issue_tokens_with_draw(client_id, scopes, None, subject, draw_security_identifier)
    }

    fn prepare_token_pair_with_draw<F, E>(
        &self,
        client_id: &str,
        scopes: &[String],
        resource: Option<&str>,
        subject: Option<&str>,
        registration_epoch: OAuthRegistrationEpoch,
        grant_id: Option<OAuthGrantId>,
        family_expires_at: Option<Instant>,
        mut draw: F,
    ) -> Result<PreparedTokenPair, OAuthError>
    where
        F: FnMut() -> Result<SecurityIdentifier, E>,
        E: std::fmt::Display,
    {
        let access_value = generate_token_with_draw(&mut draw)?;
        let refresh_value = generate_token_with_draw(&mut draw)?;
        let access_digest = digest_credential(CredentialKind::AccessToken, &access_value)?;
        let refresh_digest = digest_credential(CredentialKind::RefreshToken, &refresh_value)?;
        // Credential generation may block behind the platform RNG. Token and
        // family lifetimes begin only after both successful draws.
        let issued_at = Instant::now();
        let grant_id = match grant_id {
            Some(grant_id) => grant_id,
            None => derive_direct_grant_id(access_digest, refresh_digest)?,
        };
        let family_expires_at = match family_expires_at {
            Some(expires_at) => expires_at,
            None => checked_deadline(
                issued_at,
                self.config.refresh_token_lifetime,
                "refresh_token_lifetime",
            )?,
        };
        if family_expires_at
            .saturating_duration_since(issued_at)
            .as_secs()
            == 0
        {
            return Err(invalid_grant_error());
        }
        let access_expires_at = checked_deadline(
            issued_at,
            self.config.access_token_lifetime,
            "access_token_lifetime",
        )?
        .min(family_expires_at);
        let access_lifetime_secs = access_expires_at
            .saturating_duration_since(issued_at)
            .as_secs();
        if access_lifetime_secs == 0 {
            return Err(invalid_grant_error());
        }

        Ok(PreparedTokenPair {
            access_value,
            access_digest,
            access_metadata: StoredOAuthToken {
                metadata: OAuthToken {
                    token: String::new(),
                    token_type: TokenType::Bearer,
                    client_id: client_id.to_string(),
                    scopes: scopes.to_vec(),
                    resource: resource.map(String::from),
                    issued_at,
                    expires_at: access_expires_at,
                    subject: subject.map(String::from),
                    is_refresh_token: false,
                },
                grant_id,
                registration_epoch,
                family_expires_at,
            },
            refresh_value,
            refresh_digest,
            refresh_metadata: StoredOAuthToken {
                metadata: OAuthToken {
                    token: String::new(),
                    token_type: TokenType::Bearer,
                    client_id: client_id.to_string(),
                    scopes: scopes.to_vec(),
                    resource: resource.map(String::from),
                    issued_at,
                    expires_at: family_expires_at,
                    subject: subject.map(String::from),
                    is_refresh_token: true,
                },
                grant_id,
                registration_epoch,
                family_expires_at,
            },
            access_lifetime_secs,
        })
    }

    fn issue_tokens_with_draw<F, E>(
        &self,
        client_id: &str,
        scopes: &[String],
        resource: Option<&str>,
        subject: Option<&str>,
        mut draw: F,
    ) -> Result<TokenResponse, OAuthError>
    where
        F: FnMut() -> Result<SecurityIdentifier, E>,
        E: std::fmt::Display,
    {
        validate_optional_authorization_resource(resource)?;
        validate_optional_authorization_subject(subject)?;
        let scopes = canonicalize_request_scopes(scopes)?;
        let (mut state, _) = self.state_for_mutation()?;
        let registration_epoch = state
            .clients
            .get(client_id)
            .map(|client| client.registration_epoch)
            .ok_or_else(|| OAuthError::InvalidClient(OAUTH_CLIENT_NOT_FOUND_ERROR.to_string()))?;
        ensure_capacity(
            state.access_tokens.len(),
            state.access_token_count_for_client(client_id),
            self.config.max_access_tokens,
            self.config.max_access_tokens_per_client,
            "access tokens",
        )?;
        ensure_capacity(
            state.refresh_tokens.len(),
            state.refresh_token_count_for_client(client_id),
            self.config.max_refresh_tokens,
            self.config.max_refresh_tokens_per_client,
            "refresh tokens",
        )?;

        let prepared = self.prepare_token_pair_with_draw(
            client_id,
            &scopes,
            resource,
            subject,
            registration_epoch,
            None,
            None,
            &mut draw,
        )?;
        ensure_fresh_token_pair(&state, &prepared)?;
        let (response, access, refresh) = prepared.into_response_and_records();
        state.access_tokens.insert(access.0, access.1);
        state.refresh_tokens.insert(refresh.0, refresh.1);
        Ok(response)
    }

    // -------------------------------------------------------------------------
    // Token Revocation (RFC 7009)
    // -------------------------------------------------------------------------

    /// Revokes a token (access or refresh).
    ///
    /// Per RFC 7009, this always returns success even if the token was not found.
    pub fn revoke(
        &self,
        token: &str,
        client_id: &str,
        client_secret: Option<&str>,
    ) -> Result<(), OAuthError> {
        validate_client_authentication_admission(client_id, client_secret)?;
        if token.len() > OAUTH_OPAQUE_CREDENTIAL_BYTES || token.chars().any(char::is_control) {
            return Err(OAuthError::InvalidRequest(
                "revocation token is outside admitted bounds".to_string(),
            ));
        }
        let admitted_token = validate_opaque_credential(token, "revocation token is invalid")
            .is_ok()
            .then(|| {
                Ok::<_, OAuthError>((
                    digest_credential(CredentialKind::AccessToken, token)?,
                    digest_credential(CredentialKind::RefreshToken, token)?,
                ))
            })
            .transpose()?;
        let (mut state, now, _) =
            self.state_for_authenticated_mutation(client_id, client_secret)?;

        // Perform the ownership check and deletion under the same write lock.
        // In particular, never remove first and discover afterward that the
        // token belongs to another client.
        let Some((access_digest, refresh_digest)) = admitted_token else {
            return Ok(());
        };

        let access = state.access_tokens.get(&access_digest).cloned();
        let refresh = state.refresh_tokens.get(&refresh_digest).cloned();
        let refresh_tombstone = state.revoked_tokens.get(&refresh_digest).cloned();
        let access_owner = access.as_ref().map(|entry| &entry.client_id);
        let refresh_owner = refresh.as_ref().map(|entry| &entry.client_id);
        let refresh_tombstone_owner = refresh_tombstone.as_ref().map(|entry| &entry.client_id);
        if access_owner.is_some_and(|owner| owner != client_id)
            || refresh_owner.is_some_and(|owner| owner != client_id)
            || refresh_tombstone_owner.is_some_and(|owner| owner != client_id)
        {
            // RFC 7009 requires an indistinguishable success response for an
            // unknown token. Treat a token owned by another client the same way.
            return Ok(());
        }

        if let Some(access) = state.access_tokens.remove(&access_digest) {
            state.insert_tombstone_bounded(
                access_digest,
                RevocationTombstone {
                    client_id: client_id.to_string(),
                    grant_id: access.grant_id,
                    replay_guard: false,
                    expires_at: access.expires_at,
                },
                &self.config,
                now,
            );
        }
        let refresh_grant_id = refresh
            .as_ref()
            .map(|entry| entry.grant_id)
            .or_else(|| refresh_tombstone.map(|entry| entry.grant_id));
        if let Some(grant_id) = refresh_grant_id {
            state.revoke_grant_family(grant_id, client_id, &self.config, now);
        }

        Ok(())
    }

    // -------------------------------------------------------------------------
    // Token Introspection
    // -------------------------------------------------------------------------

    /// Validates an access token and returns its metadata.
    ///
    /// This is used internally and by the [`OAuthTokenVerifier`].
    pub fn validate_access_token(&self, token: &str) -> Option<OAuthToken> {
        self.validate_stored_access_token(token)
            .map(|stored| stored.metadata)
    }

    fn validate_stored_access_token(&self, token: &str) -> Option<StoredOAuthToken> {
        let token_digest = validate_and_digest_opaque_credential(
            CredentialKind::AccessToken,
            token,
            "access token is invalid",
        )
        .ok()?;
        let state = self.state.read().ok()?;

        // Check if revoked
        if state.revoked_tokens.contains_key(&token_digest) {
            return None;
        }

        let token_info = state.access_tokens.get(&token_digest)?;

        if token_info.is_expired() || token_info.family_expires_at <= Instant::now() {
            return None;
        }

        let current_client = state.clients.get(&token_info.client_id)?;
        if current_client.registration_epoch != token_info.registration_epoch {
            return None;
        }

        Some(token_info.clone())
    }

    // -------------------------------------------------------------------------
    // MCP Integration
    // -------------------------------------------------------------------------

    /// Creates a token verifier for one configured protected MCP resource.
    ///
    /// The expected resource must come from trusted deployment configuration,
    /// not from a request header, parameter, or token claim. Tokens without
    /// this exact audience are rejected before authentication facts are made
    /// available to [`crate::auth::TokenAuthProvider`] consumers.
    #[must_use]
    pub fn token_verifier(self: &Arc<Self>, resource: CanonicalResourceId) -> OAuthTokenVerifier {
        OAuthTokenVerifier {
            server: Arc::clone(self),
            resource,
        }
    }

    // -------------------------------------------------------------------------
    // Maintenance
    // -------------------------------------------------------------------------

    /// Removes expired tokens, authorization codes, and revocation tombstones.
    ///
    /// Mutating operations already perform this cleanup opportunistically.
    /// Call this during read-only workloads when prompt reclamation matters.
    pub fn cleanup_expired(&self) {
        let Ok(mut state) = self.state.write() else {
            return;
        };

        state.cleanup_expired_at(Instant::now());
    }

    /// Returns statistics about the server state.
    #[must_use]
    pub fn stats(&self) -> OAuthServerStats {
        let state = match self.state.read() {
            Ok(guard) => guard,
            // Preserve observability during partial failure instead of panicking on poison.
            Err(poisoned) => poisoned.into_inner(),
        };
        OAuthServerStats {
            clients: state.clients.len(),
            authorization_codes: state.authorization_codes.len(),
            access_tokens: state.access_tokens.len(),
            refresh_tokens: state.refresh_tokens.len(),
            revoked_tokens: state.revoked_tokens.len(),
        }
    }
}

/// Statistics about the OAuth server state.
#[derive(Debug, Clone, Default)]
pub struct OAuthServerStats {
    /// Number of registered clients.
    pub clients: usize,
    /// Number of pending authorization codes.
    pub authorization_codes: usize,
    /// Number of active access tokens.
    pub access_tokens: usize,
    /// Number of active refresh tokens.
    pub refresh_tokens: usize,
    /// Number of retained revocation tombstones.
    pub revoked_tokens: usize,
}

// =============================================================================
// Token Verifier Implementation
// =============================================================================

/// OAuth token verifier for MCP integration.
///
/// This implements [`TokenVerifier`] to allow the OAuth server to be used
/// with the MCP server's [`crate::auth::TokenAuthProvider`].
pub struct OAuthTokenVerifier {
    server: Arc<OAuthServer>,
    resource: CanonicalResourceId,
}

impl TokenVerifier for OAuthTokenVerifier {
    fn verify(
        &self,
        _ctx: &McpContext,
        _request: AuthRequest<'_>,
        token: &AccessToken,
    ) -> McpResult<AuthContext> {
        // Only accept Bearer tokens
        if !token.scheme.eq_ignore_ascii_case("Bearer") {
            return Err(McpError::new(
                McpErrorCode::ResourceForbidden,
                "unsupported auth scheme",
            ));
        }

        // Validate the token
        let stored_token = self
            .server
            .validate_stored_access_token(&token.token)
            .ok_or_else(|| {
                McpError::new(McpErrorCode::ResourceForbidden, "invalid or expired token")
            })?;
        if stored_token.resource.as_deref() != Some(self.resource.as_str()) {
            return Err(McpError::new(
                McpErrorCode::ResourceForbidden,
                "token is not authorized for this resource",
            ));
        }
        let registration_epoch = stored_token.registration_epoch;
        let token_info = stored_token.metadata;
        let OAuthToken {
            client_id,
            scopes,
            resource,
            subject,
            ..
        } = token_info;
        let session_owner = oauth_session_owner(
            &self.server.config.issuer,
            &client_id,
            registration_epoch,
            subject.as_deref(),
            resource.as_deref(),
        )?;
        let display_subject = subject
            .clone()
            .filter(|subject| !subject.is_empty())
            .unwrap_or_else(|| client_id.clone());

        let mut auth = AuthContext::with_subject(display_subject);
        auth.scopes = scopes;
        auth.claims = Some(serde_json::json!({
            "client_id": client_id,
            "grant_subject": subject,
            "resource": resource,
            "iss": self.server.config.issuer,
        }));
        Ok(auth.with_session_owner(session_owner))
    }
}

// =============================================================================
// Helper Functions
// =============================================================================

fn oauth_session_owner(
    issuer: &str,
    client_id: &str,
    registration_epoch: OAuthRegistrationEpoch,
    subject: Option<&str>,
    resource: Option<&str>,
) -> McpResult<Sha256Digest> {
    if issuer.len() > MAX_OAUTH_ISSUER_BYTES
        || client_id.is_empty()
        || client_id.len() > MAX_OAUTH_CLIENT_ID_BYTES
        || subject
            .is_some_and(|subject| subject.is_empty() || subject.len() > MAX_OAUTH_SUBJECT_BYTES)
        || resource.is_some_and(|resource| {
            resource.is_empty() || resource.len() > MAX_OAUTH_RESOURCE_BYTES
        })
    {
        return Err(McpError::internal_error(
            "OAuth session owner facts are outside admitted bounds",
        ));
    }

    let subject_bytes = subject.map_or(0, str::len);
    let resource_bytes = resource.map_or(0, str::len);
    let capacity = OAUTH_SESSION_OWNER_DOMAIN
        .len()
        .checked_add(8)
        .and_then(|size| size.checked_add(issuer.len()))
        .and_then(|size| size.checked_add(8))
        .and_then(|size| size.checked_add(client_id.len()))
        .and_then(|size| size.checked_add(OAUTH_REGISTRATION_EPOCH_BYTES))
        .and_then(|size| size.checked_add(1))
        .and_then(|size| size.checked_add(8))
        .and_then(|size| size.checked_add(subject_bytes))
        .and_then(|size| size.checked_add(1))
        .and_then(|size| size.checked_add(8))
        .and_then(|size| size.checked_add(resource_bytes))
        .filter(|size| *size <= MAX_OAUTH_SESSION_OWNER_INPUT_BYTES)
        .ok_or_else(|| McpError::internal_error("OAuth session owner framing overflow"))?;
    let mut framed = Vec::new();
    framed
        .try_reserve_exact(capacity)
        .map_err(|_| McpError::internal_error("OAuth session owner allocation failed"))?;
    framed.extend_from_slice(OAUTH_SESSION_OWNER_DOMAIN);
    framed.extend_from_slice(
        &u64::try_from(issuer.len())
            .map_err(|_| McpError::internal_error("OAuth issuer length overflow"))?
            .to_be_bytes(),
    );
    framed.extend_from_slice(issuer.as_bytes());
    framed.extend_from_slice(
        &u64::try_from(client_id.len())
            .map_err(|_| McpError::internal_error("OAuth client ID length overflow"))?
            .to_be_bytes(),
    );
    framed.extend_from_slice(client_id.as_bytes());
    framed.extend_from_slice(registration_epoch.as_bytes());
    match subject {
        None => framed.push(0),
        Some(subject) => {
            framed.push(1);
            framed.extend_from_slice(
                &u64::try_from(subject.len())
                    .map_err(|_| McpError::internal_error("OAuth subject length overflow"))?
                    .to_be_bytes(),
            );
            framed.extend_from_slice(subject.as_bytes());
        }
    }
    match resource {
        None => framed.push(0),
        Some(resource) => {
            framed.push(1);
            framed.extend_from_slice(
                &u64::try_from(resource.len())
                    .map_err(|_| McpError::internal_error("OAuth resource length overflow"))?
                    .to_be_bytes(),
            );
            framed.extend_from_slice(resource.as_bytes());
        }
    }

    sha256_bounded(&framed, MAX_OAUTH_SESSION_OWNER_INPUT_BYTES)
        .map_err(|_| McpError::internal_error("OAuth session owner derivation failed"))
}

fn checked_deadline(
    now: Instant,
    lifetime: Duration,
    field: &'static str,
) -> Result<Instant, OAuthError> {
    now.checked_add(lifetime).ok_or_else(|| {
        OAuthError::ServerError(format!(
            "OAuth configuration lifetime `{field}` exceeds monotonic-clock range"
        ))
    })
}

fn validate_lifetime(
    lifetime: Duration,
    minimum: Duration,
    maximum: Duration,
    field: &'static str,
) -> Result<(), OAuthError> {
    if lifetime < minimum || lifetime > maximum {
        return Err(OAuthError::ServerError(format!(
            "OAuth configuration lifetime `{field}` must be at least {} seconds and no greater \
             than {} seconds",
            minimum.as_secs(),
            maximum.as_secs()
        )));
    }
    Ok(())
}

fn capacity_error(resource: &'static str) -> OAuthError {
    OAuthError::TemporarilyUnavailable(format!("OAuth {resource} capacity has been reached"))
}

fn invalid_grant_error() -> OAuthError {
    OAuthError::InvalidGrant(OAUTH_INVALID_GRANT_ERROR.to_string())
}

fn ensure_capacity(
    global_count: usize,
    client_count: usize,
    global_limit: usize,
    client_limit: usize,
    resource: &'static str,
) -> Result<(), OAuthError> {
    if global_count >= global_limit || client_count >= client_limit {
        return Err(capacity_error(resource));
    }
    Ok(())
}

fn validate_optional_authorization_value(
    value: Option<&str>,
    max_bytes: usize,
    error: &'static str,
) -> Result<(), OAuthError> {
    if value.is_some_and(|value| value.len() > max_bytes || value.chars().any(char::is_control)) {
        return Err(OAuthError::InvalidRequest(error.to_string()));
    }
    Ok(())
}

fn validate_optional_authorization_subject(subject: Option<&str>) -> Result<(), OAuthError> {
    if subject.is_some_and(str::is_empty) {
        return Err(OAuthError::InvalidRequest(
            OAUTH_AUTHORIZATION_SUBJECT_RETENTION_ERROR.to_string(),
        ));
    }
    validate_optional_authorization_value(
        subject,
        MAX_OAUTH_SUBJECT_BYTES,
        OAUTH_AUTHORIZATION_SUBJECT_RETENTION_ERROR,
    )
}

fn validate_authorization_subject(subject: &str) -> Result<(), OAuthError> {
    if subject.is_empty() {
        return Err(OAuthError::InvalidRequest(
            OAUTH_AUTHORIZATION_SUBJECT_RETENTION_ERROR.to_string(),
        ));
    }
    validate_optional_authorization_value(
        Some(subject),
        MAX_OAUTH_SUBJECT_BYTES,
        OAUTH_AUTHORIZATION_SUBJECT_RETENTION_ERROR,
    )
}

fn validate_optional_authorization_resource(resource: Option<&str>) -> Result<(), OAuthError> {
    validate_optional_authorization_value(
        resource,
        MAX_OAUTH_RESOURCE_BYTES,
        OAUTH_AUTHORIZATION_RESOURCE_RETENTION_ERROR,
    )?;
    if let Some(resource) = resource {
        let url = Url::parse(resource).map_err(|_| {
            OAuthError::InvalidRequest(OAUTH_AUTHORIZATION_RESOURCE_RETENTION_ERROR.to_string())
        })?;
        if url.cannot_be_a_base() || url.has_authority() && url.host().is_none() {
            return Err(OAuthError::InvalidRequest(
                OAUTH_AUTHORIZATION_RESOURCE_RETENTION_ERROR.to_string(),
            ));
        }
    }
    Ok(())
}

fn is_valid_oauth_scope_token(scope: &str) -> bool {
    !scope.is_empty()
        && scope.len() <= MAX_OAUTH_SCOPE_BYTES
        && scope
            .bytes()
            .all(|byte| matches!(byte, 0x21 | 0x23..=0x5B | 0x5D..=0x7E))
}

fn canonicalize_request_scopes(scopes: &[String]) -> Result<Vec<String>, OAuthError> {
    if scopes.len() > MAX_OAUTH_SCOPES_PER_CLIENT {
        return Err(OAuthError::InvalidScope(
            OAUTH_REQUEST_SCOPE_COUNT_ERROR.to_string(),
        ));
    }
    if scopes
        .iter()
        .any(|scope| !is_valid_oauth_scope_token(scope))
    {
        return Err(OAuthError::InvalidScope(
            OAUTH_REQUEST_SCOPE_VALUE_ERROR.to_string(),
        ));
    }

    let mut seen = HashSet::with_capacity(scopes.len());
    let mut canonical = Vec::with_capacity(scopes.len());
    for scope in scopes {
        if seen.insert(scope.as_str()) {
            canonical.push(scope.clone());
        }
    }
    Ok(canonical)
}

fn ensure_fresh_token_pair(
    state: &OAuthServerState,
    pair: &PreparedTokenPair,
) -> Result<(), OAuthError> {
    if constant_time_eq(&pair.access_value, &pair.refresh_value)
        || state.credential_value_in_use(&pair.access_value)
        || state.credential_value_in_use(&pair.refresh_value)
    {
        return Err(OAuthError::ServerError(
            "generated OAuth credential collided with retained state".to_string(),
        ));
    }
    Ok(())
}

fn validate_client_id_admission(client_id: &str) -> Result<(), OAuthError> {
    if client_id.is_empty()
        || client_id.len() > MAX_OAUTH_CLIENT_ID_BYTES
        || client_id.chars().any(char::is_control)
    {
        return Err(OAuthError::InvalidClient(
            OAUTH_CLIENT_AUTHENTICATION_ERROR.to_string(),
        ));
    }
    Ok(())
}

fn validate_client_authentication_admission(
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<(), OAuthError> {
    validate_client_id_admission(client_id)?;
    if client_secret.is_some_and(|secret| secret.len() > MAX_OAUTH_CLIENT_CREDENTIAL_BYTES) {
        return Err(OAuthError::InvalidClient(
            OAUTH_CLIENT_AUTHENTICATION_ERROR.to_string(),
        ));
    }
    Ok(())
}

fn authenticate_client_or_dummy(
    state: &OAuthServerState,
    client_id: &str,
    client_secret: Option<&str>,
) -> Result<OAuthRegistrationEpoch, OAuthError> {
    let client = state.clients.get(client_id);
    let authenticated = client.map_or_else(
        || {
            let provided = client_secret.unwrap_or_default();
            perform_dummy_client_secret_verification(provided.as_bytes());
            false
        },
        |client| client.authenticate(client_secret),
    );
    if !authenticated {
        return Err(OAuthError::InvalidClient(
            OAUTH_CLIENT_AUTHENTICATION_ERROR.to_string(),
        ));
    }
    client
        .map(|client| client.registration_epoch)
        .ok_or_else(|| OAuthError::InvalidClient(OAUTH_CLIENT_AUTHENTICATION_ERROR.to_string()))
}

fn perform_dummy_client_secret_verification(provided: &[u8]) {
    let verified = ClientSecretVerifier::dummy().verify(std::hint::black_box(provided));
    std::hint::black_box(verified);
}

fn validate_opaque_credential(value: &str, error: &'static str) -> Result<(), OAuthError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    if value.len() != OAUTH_OPAQUE_CREDENTIAL_BYTES {
        return Err(OAuthError::InvalidGrant(error.to_string()));
    }
    let decoded = Zeroizing::new(
        URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| OAuthError::InvalidGrant(error.to_string()))?,
    );
    let canonical = Zeroizing::new(base64url_encode(&decoded));
    if decoded.len() != 32 || canonical.as_str() != value {
        return Err(OAuthError::InvalidGrant(error.to_string()));
    }
    Ok(())
}

fn validate_and_digest_opaque_credential(
    kind: CredentialKind,
    value: &str,
    error: &'static str,
) -> Result<CredentialDigest, OAuthError> {
    validate_opaque_credential(value, error)?;
    digest_credential(kind, value)
}

fn digest_credential(kind: CredentialKind, value: &str) -> Result<CredentialDigest, OAuthError> {
    if value.len() != OAUTH_OPAQUE_CREDENTIAL_BYTES {
        return Err(OAuthError::InvalidGrant(
            "OAuth credential is outside admitted bounds".to_string(),
        ));
    }
    let domain = kind.domain();
    let mut framed = Zeroizing::new(Vec::with_capacity(domain.len() + value.len()));
    framed.extend_from_slice(domain);
    framed.extend_from_slice(value.as_bytes());
    let digest = sha256_bounded(&framed, MAX_OPAQUE_CREDENTIAL_DIGEST_INPUT_BYTES)
        .map_err(|error| OAuthError::ServerError(error.to_string()))?;
    Ok(CredentialDigest(digest))
}

fn derive_authorization_grant_id(
    authorization_code: CredentialDigest,
) -> Result<OAuthGrantId, OAuthError> {
    derive_grant_id(AUTHORIZATION_GRANT_ID_DOMAIN, &[authorization_code])
}

fn derive_direct_grant_id(
    access_token: CredentialDigest,
    refresh_token: CredentialDigest,
) -> Result<OAuthGrantId, OAuthError> {
    derive_grant_id(DIRECT_GRANT_ID_DOMAIN, &[access_token, refresh_token])
}

fn derive_grant_id(
    domain: &'static [u8],
    credentials: &[CredentialDigest],
) -> Result<OAuthGrantId, OAuthError> {
    let mut framed = Zeroizing::new(Vec::with_capacity(domain.len() + credentials.len() * 32));
    framed.extend_from_slice(domain);
    for credential in credentials {
        framed.extend_from_slice(credential.0.as_bytes());
    }
    let digest = sha256_bounded(&framed, MAX_GRANT_ID_DERIVATION_INPUT_BYTES)
        .map_err(|error| OAuthError::ServerError(error.to_string()))?;
    Ok(OAuthGrantId::from_bytes(digest.into_bytes()))
}

fn client_secret_digest(
    salt: &[u8; CLIENT_SECRET_SALT_BYTES],
    secret: &[u8],
) -> Result<Sha256Digest, OAuthError> {
    if secret.len() > MAX_OAUTH_CLIENT_CREDENTIAL_BYTES {
        return Err(OAuthError::InvalidClient(
            OAUTH_CLIENT_AUTHENTICATION_ERROR.to_string(),
        ));
    }
    let mut framed = Zeroizing::new(Vec::with_capacity(
        CLIENT_SECRET_VERIFIER_DOMAIN.len() + salt.len() + secret.len(),
    ));
    framed.extend_from_slice(CLIENT_SECRET_VERIFIER_DOMAIN);
    framed.extend_from_slice(salt);
    framed.extend_from_slice(secret);
    sha256_bounded(&framed, MAX_CLIENT_SECRET_VERIFIER_INPUT_BYTES)
        .map_err(|error| OAuthError::ServerError(error.to_string()))
}

/// Draws token material through the core security-identifier API.
fn generate_token() -> Result<String, OAuthError> {
    generate_token_with_draw(draw_security_identifier)
}

fn generate_token_with_draw<F, E>(draw: F) -> Result<String, OAuthError>
where
    F: FnOnce() -> Result<SecurityIdentifier, E>,
    E: std::fmt::Display,
{
    let identifier = draw().map_err(|error| OAuthError::ServerError(error.to_string()))?;
    // Base64url encode (URL-safe, no padding).
    Ok(base64url_encode(identifier.as_bytes()))
}

/// Base64url encodes bytes (URL-safe, no padding).
fn base64url_encode(data: &[u8]) -> String {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    URL_SAFE_NO_PAD.encode(data)
}

/// Validates the fixed RFC 7636 verifier grammar and byte bounds.
fn validate_pkce_code_verifier(verifier: &str) -> Result<(), OAuthError> {
    let verifier_bytes = verifier.as_bytes();
    if !(PKCE_CODE_VERIFIER_MIN_BYTES..=PKCE_CODE_VERIFIER_MAX_BYTES)
        .contains(&verifier_bytes.len())
    {
        return Err(OAuthError::InvalidRequest(format!(
            "code_verifier must be {PKCE_CODE_VERIFIER_MIN_BYTES} to \
             {PKCE_CODE_VERIFIER_MAX_BYTES} bytes of RFC 7636 unreserved ASCII"
        )));
    }
    if !verifier_bytes
        .iter()
        .copied()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~'))
    {
        return Err(OAuthError::InvalidRequest(format!(
            "code_verifier must be {PKCE_CODE_VERIFIER_MIN_BYTES} to \
             {PKCE_CODE_VERIFIER_MAX_BYTES} bytes of RFC 7636 unreserved ASCII"
        )));
    }

    Ok(())
}

/// Computes the exact RFC 7636 S256 challenge from an admitted verifier.
fn compute_s256_challenge(verifier: &str) -> Result<String, OAuthError> {
    validate_pkce_code_verifier(verifier)?;
    let digest = sha256_bounded(verifier.as_bytes(), PKCE_CODE_VERIFIER_MAX_BYTES)
        .map_err(|error| OAuthError::InvalidRequest(error.to_string()))?;
    Ok(base64url_encode(digest.as_bytes()))
}

/// Validates the canonical, unpadded base64url encoding of a SHA-256 digest.
fn validate_s256_code_challenge(challenge: &str) -> Result<(), OAuthError> {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    // An unpadded base64url encoding of a 32-byte SHA-256 digest is exactly 43
    // bytes. Check this before decoding to keep the allocation strictly bounded.
    if challenge.len() != 43 {
        return Err(OAuthError::InvalidRequest(
            "code_challenge must be a canonical S256 challenge".to_string(),
        ));
    }

    let decoded = URL_SAFE_NO_PAD.decode(challenge).map_err(|_| {
        OAuthError::InvalidRequest("code_challenge must be a canonical S256 challenge".to_string())
    })?;
    if decoded.len() != 32 || base64url_encode(&decoded) != challenge {
        return Err(OAuthError::InvalidRequest(
            "code_challenge must be a canonical S256 challenge".to_string(),
        ));
    }

    Ok(())
}

/// URL-encodes a string.
fn url_encode(s: &str) -> String {
    let mut result = String::with_capacity(s.len() * 3);
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                result.push(byte as char);
            }
            _ => {
                result.push('%');
                result.push_str(&format!("{:02X}", byte));
            }
        }
    }
    result
}

fn validate_registered_redirect_uri(redirect_uris: &[String], uri: &str) -> bool {
    let Some(candidate) = parse_redirect_uri(uri) else {
        return false;
    };

    for allowed in redirect_uris {
        let Some(registered) = parse_redirect_uri(allowed) else {
            continue;
        };

        // Non-loopback redirects use byte-for-byte registration matching.
        // Parsed comparison alone would silently normalize meaningful URI
        // spelling differences at a security boundary.
        if allowed == uri {
            return true;
        }

        // RFC 8252 permits a native app to choose its loopback listener port at
        // launch. Every other component, including the exact IP family/address,
        // remains bound to the registration.
        if registered.scheme() == "http"
            && candidate.scheme() == "http"
            && loopback_redirect_match(allowed, uri)
        {
            return true;
        }
    }

    false
}

fn validate_registered_scopes(allowed_scopes: &HashSet<String>, scopes: &[String]) -> bool {
    scopes.iter().all(|scope| allowed_scopes.contains(scope))
}

fn authenticate_client_secret(expected: &[u8], provided: &[u8]) -> bool {
    let Ok(expected_digest) = sha256_bounded(expected, MAX_OAUTH_CLIENT_CREDENTIAL_BYTES) else {
        return false;
    };
    let Ok(provided_digest) = sha256_bounded(provided, MAX_OAUTH_CLIENT_CREDENTIAL_BYTES) else {
        return false;
    };

    constant_time_digest_eq(expected_digest.as_bytes(), provided_digest.as_bytes())
}

fn constant_time_digest_eq(expected: &[u8; 32], provided: &[u8; 32]) -> bool {
    let difference = expected
        .iter()
        .zip(provided)
        .fold(0_u8, |difference, (expected, provided)| {
            difference | (*expected ^ *provided)
        });
    difference == 0
}

/// Constant-time string comparison.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }

    let mut result = 0u8;
    for (x, y) in a.bytes().zip(b.bytes()) {
        result |= x ^ y;
    }
    result == 0
}

fn contains_unsafe_display_character(value: &str) -> bool {
    value.chars().any(|character| {
        character.is_control()
            || matches!(
                character,
                '\u{061c}'
                    | '\u{200e}'
                    | '\u{200f}'
                    | '\u{2028}'..='\u{202e}'
                    | '\u{2066}'..='\u{206f}'
            )
    })
}

fn parsed_url_has_credentials(url: &Url) -> bool {
    !url.username().is_empty() || url.password().is_some()
}

fn raw_url_authority(value: &str) -> Option<&str> {
    let (_, after_scheme) = value.split_once("://")?;
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    Some(&after_scheme[..authority_end])
}

fn raw_url_authority_has_userinfo(value: &str) -> bool {
    raw_url_authority(value).is_some_and(|authority| authority.contains('@'))
}

fn parse_canonical_loopback_authority(authority: &str) -> Option<&str> {
    for host in ["127.0.0.1", "[::1]"] {
        if authority == host {
            return Some(host);
        }
        if let Some(port) = authority
            .strip_prefix(host)
            .and_then(|rest| rest.strip_prefix(':'))
        {
            if port.is_empty()
                || !port.bytes().all(|byte| byte.is_ascii_digit())
                || (port.len() > 1 && port.starts_with('0'))
                || port.parse::<u16>().is_err()
            {
                return None;
            }
            return Some(host);
        }
    }
    None
}

fn raw_loopback_redirect_parts(value: &str) -> Option<(&str, &str)> {
    let after_scheme = value.strip_prefix("http://")?;
    let authority_end = after_scheme
        .find(['/', '?', '#'])
        .unwrap_or(after_scheme.len());
    let authority = &after_scheme[..authority_end];
    let host = parse_canonical_loopback_authority(authority)?;
    Some((host, &after_scheme[authority_end..]))
}

fn is_literal_loopback_host(url: &Url) -> bool {
    match url.host() {
        Some(Host::Ipv4(address)) => address == std::net::Ipv4Addr::LOCALHOST,
        Some(Host::Ipv6(address)) => address == std::net::Ipv6Addr::LOCALHOST,
        Some(Host::Domain(_)) | None => false,
    }
}

/// How far a secure endpoint's spelling may differ from its `Url`
/// serialization.
#[derive(Clone, Copy, PartialEq, Eq)]
enum EndpointSpelling {
    /// Byte-for-byte equal. Redirect URIs are matched exactly, so admitting
    /// any normalisation of attacker-supplied input is an attack surface.
    Exact,
    /// Also admits a path-less authority such as `https://issuer.example`,
    /// which `Url` serializes with the root path appended. That is the RFC 8414
    /// issuer form. Issuers and endpoint bases are operator configuration and
    /// are published and compared verbatim, so no second spelling arises.
    PathlessAuthority,
}

impl EndpointSpelling {
    fn admits(self, value: &str, url: &Url) -> bool {
        let serialized = url.as_str();
        serialized == value
            || (self == Self::PathlessAuthority
                && url.path() == "/"
                && url.query().is_none()
                && serialized.strip_suffix('/') == Some(value))
    }
}

fn parse_secure_endpoint(value: &str, max_bytes: usize, spelling: EndpointSpelling) -> Option<Url> {
    if value.is_empty()
        || value.len() > max_bytes
        || value.chars().any(char::is_control)
        || raw_url_authority_has_userinfo(value)
    {
        return None;
    }

    let url = Url::parse(value).ok()?;
    if url.cannot_be_a_base()
        || url.host().is_none()
        || parsed_url_has_credentials(&url)
        || url.fragment().is_some()
        || !spelling.admits(value, &url)
    {
        return None;
    }

    match url.scheme() {
        "https" => Some(url),
        "http"
            if is_literal_loopback_host(&url)
                && raw_url_authority(value)
                    .and_then(parse_canonical_loopback_authority)
                    .is_some() =>
        {
            Some(url)
        }
        _ => None,
    }
}

pub(crate) fn validate_oauth_issuer(issuer: &str) -> Result<(), OAuthError> {
    let valid = parse_secure_endpoint(
        issuer,
        MAX_OAUTH_ISSUER_BYTES,
        EndpointSpelling::PathlessAuthority,
    )
    .is_some_and(|url| url.scheme() == "https" && url.query().is_none());
    if !valid {
        return Err(OAuthError::ServerError(OAUTH_ISSUER_ERROR.to_string()));
    }
    Ok(())
}

fn parse_redirect_uri(uri: &str) -> Option<Url> {
    let url = parse_secure_endpoint(uri, MAX_OAUTH_REDIRECT_URI_BYTES, EndpointSpelling::Exact)?;
    let has_reserved_response_parameter = url.query().is_some_and(|query| {
        query.split(['&', ';']).any(|field| {
            let raw_name = field.split_once('=').map_or(field, |(name, _)| name);
            url::form_urlencoded::parse(raw_name.as_bytes())
                .next()
                .is_some_and(|(name, _)| {
                    matches!(
                        name.as_ref(),
                        "code" | "state" | "error" | "error_description" | "error_uri" | "iss"
                    )
                })
        })
    });
    (!has_reserved_response_parameter).then_some(url)
}

fn loopback_redirect_match(registered: &str, candidate: &str) -> bool {
    match (
        raw_loopback_redirect_parts(registered),
        raw_loopback_redirect_parts(candidate),
    ) {
        (Some((registered_host, registered_tail)), Some((candidate_host, candidate_tail))) => {
            registered_host == candidate_host && registered_tail == candidate_tail
        }
        _ => false,
    }
}

#[cfg(test)]
fn is_loopback_redirect(uri: &str) -> bool {
    parse_redirect_uri(uri).is_some_and(|url| url.scheme() == "http")
}

#[cfg(test)]
fn loopback_match(a: &str, b: &str) -> bool {
    parse_redirect_uri(a).is_some()
        && parse_redirect_uri(b).is_some()
        && loopback_redirect_match(a, b)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const TEST_MCP_RESOURCE: &str = "https://resource.example/api";

    fn configured_resource(resource: &str) -> CanonicalResourceId {
        let endpoint =
            fastmcp_core::CanonicalHttpUrl::parse(resource).expect("trusted test endpoint");
        CanonicalResourceId::parse_for_endpoint(
            resource,
            &endpoint,
            fastmcp_core::CanonicalResourceIdPolicy::DEFAULT,
        )
        .expect("trusted test resource")
    }

    #[derive(Clone, Copy)]
    enum ApprovalTestMode {
        Exact,
        WrongBinding,
        WrongGeneration,
        WrongScopes,
        WrongResource,
        Denied,
        Error,
        Cancelled,
    }

    struct CountingApprovalBackend {
        generation: AuthorizationApprovalGeneration,
        mode: ApprovalTestMode,
        calls: AtomicUsize,
        observed_debug: std::sync::Mutex<Vec<String>>,
    }

    impl CountingApprovalBackend {
        fn new(mode: ApprovalTestMode) -> Self {
            Self {
                generation: AuthorizationApprovalGeneration::from_bytes([0x07; 32]),
                mode,
                calls: AtomicUsize::new(0),
                observed_debug: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl AuthorizationApprovalBackend for CountingApprovalBackend {
        fn generation(&self) -> AuthorizationApprovalGeneration {
            self.generation
        }

        fn approve(
            &self,
            request: &AuthorizationApprovalRequest,
        ) -> AuthorizationApprovalDisposition {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.observed_debug
                .lock()
                .expect("approval observation lock")
                .push(format!("{request:?}"));
            match self.mode {
                ApprovalTestMode::Denied => AuthorizationApprovalDisposition::Denied,
                ApprovalTestMode::Error => AuthorizationApprovalDisposition::Error,
                ApprovalTestMode::Cancelled => AuthorizationApprovalDisposition::Cancelled,
                mode => {
                    let mut decision = request
                        .approve(
                            "approved-subject".to_string(),
                            request.scopes().to_vec(),
                            request.resource().map(str::to_string),
                            if matches!(mode, ApprovalTestMode::WrongGeneration) {
                                AuthorizationApprovalGeneration::from_bytes([0x08; 32])
                            } else {
                                self.generation
                            },
                        )
                        .expect("validated request must construct test decision");
                    match mode {
                        ApprovalTestMode::WrongBinding => {
                            decision.binding.state = Some("wrong-state".to_string());
                        }
                        ApprovalTestMode::WrongScopes => {
                            decision.approved_scopes.push("wrong-scope".to_string());
                        }
                        ApprovalTestMode::WrongResource => {
                            decision.approved_resource = Some("https://wrong.example/".to_string());
                        }
                        ApprovalTestMode::Exact
                        | ApprovalTestMode::WrongGeneration
                        | ApprovalTestMode::Denied
                        | ApprovalTestMode::Error
                        | ApprovalTestMode::Cancelled => {}
                    }
                    AuthorizationApprovalDisposition::Approved(decision)
                }
            }
        }
    }

    fn server_with_counting_approval(backend: Arc<CountingApprovalBackend>) -> OAuthServer {
        OAuthServer::with_approval_backend(OAuthServerConfig::default(), backend)
    }

    fn configured_approved_test_server(config: OAuthServerConfig) -> OAuthServer {
        OAuthServer::with_approval_backend(
            config,
            Arc::new(TestDefaultAuthorizationApprovalBackend),
        )
    }

    fn take_parameter_value(
        admission: &mut OAuthParameterAdmission,
        name: OAuthParameterName,
    ) -> Option<String> {
        admission
            .take_defined_value(name)
            .map(OAuthSensitiveParameterValue::into_string)
    }

    fn unknown_form_with_value_lengths(value_lengths: &[usize]) -> Vec<u8> {
        assert_ne!(value_lengths.len(), 0);
        let mut input = Vec::new();
        for (index, value_len) in value_lengths.iter().copied().enumerate() {
            if index != 0 {
                input.push(b'&');
            }
            input.extend_from_slice(b"unknown=");
            input.extend(std::iter::repeat_n(b'x', value_len));
        }
        input
    }

    fn assert_oauth_stats_unchanged(before: &OAuthServerStats, after: &OAuthServerStats) {
        assert_eq!(after.clients, before.clients);
        assert_eq!(after.authorization_codes, before.authorization_codes);
        assert_eq!(after.access_tokens, before.access_tokens);
        assert_eq!(after.refresh_tokens, before.refresh_tokens);
        assert_eq!(after.revoked_tokens, before.revoked_tokens);
    }

    #[test]
    fn authorization_parameter_admission_preserves_order_and_unknowns_without_typed_effect() {
        let mut admission = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            b"response_type=code&client_id=demo&scope=read+write&state=&unknown=one&unknown=two",
        )
        .expect("bounded authorization query must be admitted");

        assert_eq!(admission.source(), OAuthParameterSource::Query);
        assert_eq!(admission.parameters().len(), 6);
        assert_eq!(admission.parameters()[0].ordinal(), 0);
        assert_eq!(
            admission.parameters()[0].source(),
            OAuthParameterSource::Query
        );
        assert_eq!(admission.parameters()[0].name(), "response_type");
        assert_eq!(admission.parameters()[2].value_len(), "read write".len());
        assert!(admission.parameters()[3].is_defined());
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::ResponseType),
            Some("code".to_string())
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::ClientId),
            Some("demo".to_string())
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::State),
            None
        );
        assert_eq!(
            admission
                .unknown_parameters()
                .map(|parameter| (parameter.ordinal(), parameter.name(), parameter.value_len()))
                .collect::<Vec<_>>(),
            vec![(4, "unknown", 3), (5, "unknown", 3)]
        );
    }

    #[test]
    fn authorization_parameter_admission_rejects_only_a_repeated_defined_name() {
        // This differs from the matching positive only by a second client_id.
        // Rejection occurs before an adapter could authenticate a client or
        // create an authorization grant.
        let error = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            b"response_type=code&client_id=demo&scope=read+write&state=&client_id=other&unknown=two",
        )
        .expect_err("a repeated defined parameter must be rejected");

        assert_eq!(
            error,
            OAuthParameterAdmissionError::DuplicateDefinedParameter {
                parameter: OAuthParameterName::ClientId,
                source: OAuthParameterSource::Query,
                first_ordinal: 1,
                duplicate_ordinal: 4,
            }
        );
    }

    #[test]
    fn form_parameter_admission_decodes_percent_and_plus_and_omits_empty_defined_values() {
        let mut admission = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"grant_type=authorization_code&client_id=demo%2Bclient&client_secret=&code_verifier=one+two%2Bthree&unknown=x&unknown=y",
        )
        .expect("strictly encoded token form must be admitted");

        assert_eq!(admission.source(), OAuthParameterSource::Form);
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::ClientId),
            Some("demo+client".to_string())
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::CodeVerifier),
            Some("one two+three".to_string())
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::ClientSecret),
            None
        );
        assert_eq!(admission.unknown_parameters().count(), 2);
    }

    #[test]
    fn form_parameter_admission_rejects_an_empty_then_nonempty_defined_duplicate() {
        // The near-identical positive above has one client_secret field. An
        // empty first occurrence still reserves that defined name, so a later
        // value cannot turn omission into an authentication ambiguity.
        let error = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"grant_type=authorization_code&client_id=demo%2Bclient&client_secret=&client_secret=secret&code_verifier=one+two%2Bthree&unknown=x&unknown=y",
        )
        .expect_err("empty defined values must not evade duplicate rejection");

        assert_eq!(
            error,
            OAuthParameterAdmissionError::DuplicateDefinedParameter {
                parameter: OAuthParameterName::ClientSecret,
                source: OAuthParameterSource::Form,
                first_ordinal: 2,
                duplicate_ordinal: 3,
            }
        );
    }

    #[test]
    fn resource_is_a_defined_singleton_for_authorization_and_token_profiles() {
        let resource = "https%3A%2F%2Fresource.example%2Fapi";
        let mut authorization = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            format!("client_id=demo&resource={resource}").as_bytes(),
        )
        .expect("authorization resource must be admitted");
        assert_eq!(
            take_parameter_value(&mut authorization, OAuthParameterName::Resource),
            Some("https://resource.example/api".to_string())
        );

        let mut token = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            format!("grant_type=refresh_token&resource={resource}").as_bytes(),
        )
        .expect("token resource must be admitted");
        assert_eq!(
            take_parameter_value(&mut token, OAuthParameterName::Resource),
            Some("https://resource.example/api".to_string())
        );
    }

    #[test]
    fn token_resource_duplicate_is_rejected_before_grant_processing() {
        // This differs from the matching token positive only by a second
        // resource field. It must not become an ambiguous resource selector.
        let error = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"grant_type=refresh_token&resource=https%3A%2F%2Fresource.example%2Fa&resource=https%3A%2F%2Fresource.example%2Fb",
        )
        .expect_err("repeated RFC 8707 resource must be rejected");
        assert_eq!(
            error,
            OAuthParameterAdmissionError::DuplicateDefinedParameter {
                parameter: OAuthParameterName::Resource,
                source: OAuthParameterSource::Form,
                first_ordinal: 1,
                duplicate_ordinal: 2,
            }
        );
    }

    #[test]
    fn parameter_admission_ignores_empty_segments_and_treats_bare_names_as_empty() {
        let mut admission = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            b"&&state&&client_id=demo&scope&unknown&unknown=&&",
        )
        .expect("empty segments and bare names are standard form syntax");

        assert_eq!(admission.parameters().len(), 5);
        assert_eq!(admission.parameters()[0].ordinal(), 0);
        assert_eq!(admission.parameters()[0].name(), "state");
        assert_eq!(admission.parameters()[0].value_len(), 0);
        assert_eq!(admission.parameters()[2].name(), "scope");
        assert_eq!(admission.parameters()[2].value_len(), 0);
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::State),
            None
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::Scope),
            None
        );
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::ClientId),
            Some("demo".to_string())
        );
        assert_eq!(
            admission
                .unknown_parameters()
                .map(|parameter| (parameter.ordinal(), parameter.name(), parameter.value_len()))
                .collect::<Vec<_>>(),
            vec![(3, "unknown", 0), (4, "unknown", 0)]
        );
    }

    #[test]
    fn bare_and_equals_empty_defined_names_still_trigger_duplicate_rejection() {
        // The positive above contains one bare `state`. Adding only `state=`
        // must be rejected even though both values are omitted downstream.
        let error = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            b"&&state&state=&&client_id=demo&scope&unknown&unknown=&&",
        )
        .expect_err("empty spellings cannot evade duplicate defined-name rejection");
        assert_eq!(
            error,
            OAuthParameterAdmissionError::DuplicateDefinedParameter {
                parameter: OAuthParameterName::State,
                source: OAuthParameterSource::Query,
                first_ordinal: 0,
                duplicate_ordinal: 1,
            }
        );
    }

    #[test]
    fn parameter_admission_is_pure_and_not_an_http_or_authorization_gate() {
        // The parser accepts raw bytes only. It neither receives an OAuth
        // server nor has a route, transport, redirect, subject, or mutation
        // capability; later HTTP and AUTH-07 layers own those concerns.
        let server = OAuthServer::with_defaults();
        let before = server.stats();
        let mut admission = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"grant_type=refresh_token&refresh_token=credential&unknown=discard-me",
        )
        .expect("pure parameter admission must succeed independently of OAuth state");
        assert_eq!(
            take_parameter_value(&mut admission, OAuthParameterName::RefreshToken),
            Some("credential".to_string())
        );
        let after = server.stats();
        assert_oauth_stats_unchanged(&before, &after);
    }

    #[test]
    fn admission_diagnostics_are_redacted_for_defined_and_unknown_values() {
        let admission = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"client_secret=defined-never-prints&unknown=unknown-never-prints",
        )
        .expect("bounded input must be admitted for redaction inspection");

        let admission_debug = format!("{admission:?}");
        let parameter_debug = format!("{:?}", admission.parameters());
        for secret in ["defined-never-prints", "unknown-never-prints"] {
            assert!(!admission_debug.contains(secret));
            assert!(!parameter_debug.contains(secret));
        }
        assert!(parameter_debug.contains("value_len"));
    }

    #[test]
    fn parameter_admission_profiles_keep_defined_names_endpoint_specific() {
        let mut token_form = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::TokenForm,
            b"grant_type=refresh_token&token=opaque-value&unknown=first&unknown=second",
        )
        .expect("token form must admit bounded unknown parameters");
        assert_eq!(
            take_parameter_value(&mut token_form, OAuthParameterName::GrantType),
            Some("refresh_token".to_string())
        );
        assert_eq!(
            take_parameter_value(&mut token_form, OAuthParameterName::Token),
            None
        );
        assert_eq!(
            token_form
                .unknown_parameters()
                .map(OAuthAdmittedParameter::name)
                .collect::<Vec<_>>(),
            vec!["token", "unknown", "unknown"]
        );

        for endpoint in [
            OAuthParameterEndpoint::RevocationForm,
            OAuthParameterEndpoint::IntrospectionForm,
        ] {
            let mut form = OAuthParameterAdmission::admit(
                endpoint,
                b"token=opaque-value&token_type_hint=refresh_token&client_id=demo&client_secret=&resource=https%3A%2F%2Fresource.example%2Fapi",
            )
            .expect("token-like form must admit its defined values");
            assert_eq!(form.source(), OAuthParameterSource::Form);
            assert_eq!(
                take_parameter_value(&mut form, OAuthParameterName::Token),
                Some("opaque-value".to_string())
            );
            assert_eq!(
                take_parameter_value(&mut form, OAuthParameterName::TokenTypeHint),
                Some("refresh_token".to_string())
            );
            assert_eq!(
                take_parameter_value(&mut form, OAuthParameterName::ClientId),
                Some("demo".to_string())
            );
            assert_eq!(
                take_parameter_value(&mut form, OAuthParameterName::ClientSecret),
                None
            );
            assert_eq!(
                take_parameter_value(&mut form, OAuthParameterName::Resource),
                None
            );
            assert_eq!(
                form.unknown_parameters()
                    .map(OAuthAdmittedParameter::name)
                    .collect::<Vec<_>>(),
                vec!["resource"]
            );
        }
    }

    #[test]
    fn parameter_admission_rejects_malformed_encoding_and_controls_before_state() {
        for (input, expected) in [
            (
                b"grant_type=authorization_code&code=%".as_slice(),
                OAuthParameterAdmissionError::MalformedPercentEncoding,
            ),
            (
                b"grant_type=authorization_code&code=%GG".as_slice(),
                OAuthParameterAdmissionError::MalformedPercentEncoding,
            ),
            (
                b"grant_type=authorization_code&code=%FF".as_slice(),
                OAuthParameterAdmissionError::InvalidUtf8,
            ),
            (
                b"grant_type=authorization_code&code=%0A".as_slice(),
                OAuthParameterAdmissionError::ControlCharacter,
            ),
            (
                b"grant_type=authorization_code&code=raw\ncontrol".as_slice(),
                OAuthParameterAdmissionError::ControlCharacter,
            ),
            (
                b"grant_type=authorization_code&=code".as_slice(),
                OAuthParameterAdmissionError::EmptyName,
            ),
        ] {
            let error = OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, input)
                .expect_err("malformed token form must be rejected before endpoint logic");
            assert_eq!(error, expected);
        }
    }

    #[test]
    fn parameter_admission_accepts_exact_limits_and_rejects_each_n_plus_one_without_state() {
        let server = OAuthServer::with_defaults();
        let before = server.stats();

        // Four bounded unknown values reach exactly 16 KiB without exceeding
        // the 4 KiB decoded-value cap: 3 * (8 + 4096) + 3 + (8 + 4061).
        let exact_body = unknown_form_with_value_lengths(&[4_096, 4_096, 4_096, 4_061]);
        assert_eq!(exact_body.len(), MAX_OAUTH_FORM_BODY_BYTES);
        let form = OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &exact_body)
            .expect("exact form-body limit must be admitted");
        assert_eq!(form.parameters().len(), 4);

        let mut form_n_plus_one = exact_body.clone();
        form_n_plus_one.push(b'x');
        let error =
            OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &form_n_plus_one)
                .expect_err("form body N+1 must reject before endpoint logic");
        assert_eq!(error, OAuthParameterAdmissionError::InputTooLarge);

        assert_eq!(exact_body.len(), MAX_OAUTH_AUTHORIZATION_QUERY_BYTES);
        let query =
            OAuthParameterAdmission::admit(OAuthParameterEndpoint::AuthorizationQuery, &exact_body)
                .expect("exact authorization-query limit must be admitted");
        assert_eq!(query.parameters().len(), 4);

        let mut query_n_plus_one = exact_body.clone();
        query_n_plus_one.push(b'x');
        let error = OAuthParameterAdmission::admit(
            OAuthParameterEndpoint::AuthorizationQuery,
            &query_n_plus_one,
        )
        .expect_err("authorization query N+1 must reject before endpoint logic");
        assert_eq!(error, OAuthParameterAdmissionError::InputTooLarge);

        let exact_pair_lengths = [1; MAX_OAUTH_PARAMETER_PAIRS];
        let exact_pairs = unknown_form_with_value_lengths(&exact_pair_lengths);
        let pairs = OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &exact_pairs)
            .expect("64 nonempty pairs must be admitted");
        assert_eq!(pairs.parameters().len(), MAX_OAUTH_PARAMETER_PAIRS);

        let mut pairs_n_plus_one = exact_pairs;
        pairs_n_plus_one.extend_from_slice(b"&unknown=x");
        let error =
            OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &pairs_n_plus_one)
                .expect_err("65th nonempty pair must reject before endpoint logic");
        assert_eq!(error, OAuthParameterAdmissionError::TooManyPairs);

        let mut exact_name = vec![b'n'; MAX_OAUTH_PARAMETER_NAME_BYTES];
        exact_name.push(b'=');
        let name = OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &exact_name)
            .expect("256-byte decoded name must be admitted");
        assert_eq!(
            name.parameters()[0].name().len(),
            MAX_OAUTH_PARAMETER_NAME_BYTES
        );

        let mut name_n_plus_one = exact_name;
        name_n_plus_one.insert(MAX_OAUTH_PARAMETER_NAME_BYTES, b'n');
        let error =
            OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &name_n_plus_one)
                .expect_err("257-byte decoded name must reject before endpoint logic");
        assert_eq!(error, OAuthParameterAdmissionError::NameTooLarge);

        let exact_value = unknown_form_with_value_lengths(&[MAX_OAUTH_PARAMETER_VALUE_BYTES]);
        let value = OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &exact_value)
            .expect("4096-byte decoded value must be admitted");
        assert_eq!(
            value.parameters()[0].value_len(),
            MAX_OAUTH_PARAMETER_VALUE_BYTES
        );

        let mut value_n_plus_one = exact_value;
        value_n_plus_one.push(b'x');
        let error =
            OAuthParameterAdmission::admit(OAuthParameterEndpoint::TokenForm, &value_n_plus_one)
                .expect_err("4097-byte decoded value must reject before endpoint logic");
        assert_eq!(error, OAuthParameterAdmissionError::ValueTooLarge);

        assert_oauth_stats_unchanged(&before, &server.stats());
    }

    fn issue_access_token_via_auth_code(
        server: &OAuthServer,
        client_id: &str,
        redirect_uri: &str,
        scopes: &[&str],
        _subject: &str,
    ) -> TokenResponse {
        let code_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string();
        let code_challenge = compute_s256_challenge(&code_verifier).expect("valid verifier");
        let auth_request = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: client_id.to_string(),
            redirect_uri: redirect_uri.to_string(),
            scopes: scopes.iter().map(|scope| (*scope).to_string()).collect(),
            resource: Some(TEST_MCP_RESOURCE.to_string()),
            state: Some("oauth-test-state".to_string()),
            code_challenge,
            code_challenge_method: CodeChallengeMethod::S256,
        };

        let (code, _redirect) = server.authorize(&auth_request).expect("authorize");
        server
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                redirect_uri: Some(redirect_uri.to_string()),
                client_id: client_id.to_string(),
                client_secret: None,
                code_verifier: Some(code_verifier),
                refresh_token: None,
                scopes: None,
                resource: Some(TEST_MCP_RESOURCE.to_string()),
            })
            .expect("token exchange")
    }

    fn bounded_test_client(client_id: &str) -> OAuthClient {
        OAuthClient::builder(client_id)
            .redirect_uri("http://127.0.0.1/callback")
            .build()
            .expect("valid test client")
    }

    fn exact_ascii_value(prefix: &str, byte_len: usize) -> String {
        assert!(prefix.len() <= byte_len);
        let mut value = String::with_capacity(byte_len);
        value.push_str(prefix);
        value.extend(std::iter::repeat_n('x', byte_len - prefix.len()));
        value
    }

    fn authorization_code_digest(value: &str) -> CredentialDigest {
        digest_credential(CredentialKind::AuthorizationCode, value)
            .expect("valid authorization-code fixture")
    }

    fn access_token_digest(value: &str) -> CredentialDigest {
        digest_credential(CredentialKind::AccessToken, value).expect("valid access-token fixture")
    }

    fn refresh_token_digest(value: &str) -> CredentialDigest {
        digest_credential(CredentialKind::RefreshToken, value).expect("valid refresh-token fixture")
    }

    fn test_grant_id(tag: u8) -> OAuthGrantId {
        OAuthGrantId::from_bytes([tag; 32])
    }

    fn test_registration_epoch(tag: u8) -> OAuthRegistrationEpoch {
        OAuthRegistrationEpoch([tag; OAUTH_REGISTRATION_EPOCH_BYTES])
    }

    fn assert_registration_rejects_mutation<F>(mutate: F, expected: &str)
    where
        F: FnOnce(&mut OAuthClient),
    {
        let server = OAuthServer::with_defaults();
        let mut client = bounded_test_client("bounded-client");
        mutate(&mut client);
        let error = server
            .register_client(client)
            .expect_err("mutated client must be rejected before retention");
        assert!(matches!(&error, OAuthError::InvalidRequest(_)));
        assert_eq!(error.description(), expected);
        assert!(server.state.read().unwrap().clients.is_empty());
    }

    fn assert_client_build_error(result: Result<OAuthClient, OAuthError>, expected: &str) {
        let error = result.expect_err("out-of-bounds client must not build");
        assert!(matches!(&error, OAuthError::InvalidRequest(_)));
        assert_eq!(error.description(), expected);
    }

    fn bounded_authorization_request(client_id: &str) -> AuthorizationRequest {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: client_id.to_string(),
            redirect_uri: "http://127.0.0.1/callback".to_string(),
            scopes: Vec::new(),
            resource: None,
            state: None,
            code_challenge: compute_s256_challenge(verifier).expect("valid verifier"),
            code_challenge_method: CodeChallengeMethod::S256,
        }
    }

    fn approved_resource_request(client_id: &str) -> AuthorizationRequest {
        AuthorizationRequest {
            resource: Some("https://resource.example/api".to_string()),
            state: Some("approval-state".to_string()),
            scopes: vec!["read".to_string()],
            ..bounded_authorization_request(client_id)
        }
    }

    fn resource_code_exchange_request(client_id: &str, code: &str, resource: &str) -> TokenRequest {
        TokenRequest {
            resource: Some(resource.to_string()),
            ..bounded_code_exchange_request(client_id, code)
        }
    }

    fn resource_refresh_request(
        client_id: &str,
        refresh_token: &str,
        resource: &str,
    ) -> TokenRequest {
        TokenRequest {
            resource: Some(resource.to_string()),
            ..bounded_refresh_request(client_id, refresh_token)
        }
    }

    fn insert_expired_authorization_code_cleanup_canary(
        server: &OAuthServer,
        client_id: &str,
    ) -> CredentialDigest {
        let expired_code = base64url_encode(&[0xa7_u8; 32]);
        let digest = authorization_code_digest(&expired_code);
        let now = Instant::now();
        let mut state = server.state.write().expect("state");
        let registration_epoch = state
            .clients
            .get(client_id)
            .expect("registered client")
            .registration_epoch;
        state.authorization_codes.insert(
            digest,
            AuthorizationCode {
                client_id: client_id.to_string(),
                redirect_uri: "http://127.0.0.1/callback".to_string(),
                scopes: Vec::new(),
                resource: None,
                code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_string(),
                code_challenge_method: CodeChallengeMethod::S256,
                issued_at: now,
                expires_at: now - Duration::from_secs(1),
                subject: None,
                state: None,
                registration_epoch,
            },
        );
        digest
    }

    #[test]
    fn authorization_approval_backend_is_called_once_and_receives_only_redacted_facts() {
        const CLIENT_SECRET: &str = "approval-client-secret-canary";
        const CODE_CANARY: &str = "approval-code-canary";
        const TOKEN_CANARY: &str = "approval-token-canary";
        let backend = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let server = server_with_counting_approval(Arc::clone(&backend));
        server
            .register_client(
                OAuthClient::builder("approval-client")
                    .secret(CLIENT_SECRET)
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded confidential client"),
            )
            .expect("register client");
        let request = approved_resource_request("approval-client");
        let before = server.stats();

        let (code, _) = server.authorize(&request).expect("approved authorization");

        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            server.stats().authorization_codes,
            before.authorization_codes + 1
        );
        let state = server.state.read().expect("state");
        let retained = state
            .authorization_codes
            .get(&authorization_code_digest(&code))
            .expect("approved code retained");
        assert_eq!(retained.subject.as_deref(), Some("approved-subject"));
        assert_eq!(retained.scopes, ["read"]);
        assert_eq!(
            retained.resource.as_deref(),
            Some("https://resource.example/api")
        );
        let observed = backend
            .observed_debug
            .lock()
            .expect("observed request")
            .join("\n");
        for canary in [CLIENT_SECRET, CODE_CANARY, TOKEN_CANARY] {
            assert!(!observed.contains(canary));
        }
        assert!(observed.contains("AuthorizationApprovalRequest"));
    }

    #[test]
    fn approved_resource_survives_code_exchange_introspection_auth_and_refresh_rotation() {
        const RESOURCE: &str = "https://resource.example/api";
        let backend = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let server = Arc::new(server_with_counting_approval(Arc::clone(&backend)));
        server
            .register_client(
                OAuthClient::builder("resource-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded client"),
            )
            .expect("register client");

        let (code, _) = server
            .authorize(&approved_resource_request("resource-client"))
            .expect("approved authorization");
        let issued = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code,
                RESOURCE,
            ))
            .expect("exact resource code exchange");
        let initial_access = server
            .validate_access_token(&issued.access_token)
            .expect("issued access token introspection");
        assert_eq!(initial_access.resource.as_deref(), Some(RESOURCE));
        let refresh = issued.refresh_token.expect("refresh token");

        let auth = server
            .token_verifier(configured_resource(RESOURCE))
            .verify(
                &McpContext::new(asupersync::Cx::for_testing(), 1),
                AuthRequest {
                    method: "tools/list",
                    params: None,
                    transport_authorization: None,
                    request_id: 1,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: issued.access_token,
                },
            )
            .expect("token verifier accepts issued access token");
        assert_eq!(
            auth.claims
                .as_ref()
                .and_then(|facts| facts["resource"].as_str()),
            Some(RESOURCE)
        );

        let rotated = server
            .token(&bounded_refresh_request("resource-client", &refresh))
            .expect("omitted-resource refresh preserves the bound resource");
        let rotated_access = server
            .validate_access_token(&rotated.access_token)
            .expect("rotated access token introspection");
        assert_eq!(rotated_access.resource.as_deref(), Some(RESOURCE));
        let rotated_refresh = rotated.refresh_token.expect("rotated refresh token");
        assert_eq!(
            server
                .state
                .read()
                .expect("state")
                .refresh_tokens
                .get(&refresh_token_digest(&rotated_refresh))
                .and_then(|token| token.resource.as_deref()),
            Some(RESOURCE),
        );
        let rotated_auth = server
            .token_verifier(configured_resource(RESOURCE))
            .verify(
                &McpContext::new(asupersync::Cx::for_testing(), 1),
                AuthRequest {
                    method: "tools/list",
                    params: None,
                    transport_authorization: None,
                    request_id: 2,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: rotated.access_token.clone(),
                },
            )
            .expect("token verifier accepts rotated access token");
        assert_eq!(
            rotated_auth
                .claims
                .as_ref()
                .and_then(|facts| facts["resource"].as_str()),
            Some(RESOURCE)
        );
        assert_eq!(auth.session_owner(), rotated_auth.session_owner());
        let session_principal = crate::session::SessionPrincipalBinding::default();
        let initial_fp =
            crate::auth::principal_fingerprint(Some(&auth)).expect("initial fingerprint");
        assert!(session_principal.bind_or_verify(initial_fp));
        let rotated_fp =
            crate::auth::principal_fingerprint(Some(&rotated_auth)).expect("rotated fingerprint");
        assert!(session_principal.bind_or_verify(rotated_fp));
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn refresh_resource_mismatch_rejects_without_consuming_or_widening_the_grant() {
        const RESOURCE: &str = "https://resource.example/api";
        const WRONG_RESOURCE: &str = "https://resource.example/other";
        let backend = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let server = server_with_counting_approval(Arc::clone(&backend));
        server
            .register_client(
                OAuthClient::builder("resource-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded client"),
            )
            .expect("register client");
        let (code, _) = server
            .authorize(&approved_resource_request("resource-client"))
            .expect("approved authorization");
        let issued = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code,
                RESOURCE,
            ))
            .expect("exact resource code exchange");
        let refresh = issued.refresh_token.expect("refresh token");
        let cleanup_canary =
            insert_expired_authorization_code_cleanup_canary(&server, "resource-client");
        let before = server.stats();

        let error = server
            .token(&resource_refresh_request(
                "resource-client",
                &refresh,
                WRONG_RESOURCE,
            ))
            .expect_err("only the resource differs");

        assert!(matches!(error, OAuthError::InvalidGrant(_)));
        assert_oauth_stats_unchanged(&before, &server.stats());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        {
            let state = server.state.read().expect("state");
            assert!(state.authorization_codes.contains_key(&cleanup_canary));
            assert_eq!(
                state
                    .refresh_tokens
                    .get(&refresh_token_digest(&refresh))
                    .and_then(|token| token.resource.as_deref()),
                Some(RESOURCE)
            );
        }

        let rotated = server
            .token(&resource_refresh_request(
                "resource-client",
                &refresh,
                RESOURCE,
            ))
            .expect("unchanged refresh remains usable with its exact resource");
        assert_eq!(
            server
                .validate_access_token(&rotated.access_token)
                .and_then(|token| token.resource),
            Some(RESOURCE.to_string())
        );
    }

    #[test]
    fn authorization_code_resource_mismatch_rejects_before_expiry_cleanup() {
        const RESOURCE: &str = "https://resource.example/api";
        const WRONG_RESOURCE: &str = "https://resource.example/other";
        let backend = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let server = server_with_counting_approval(Arc::clone(&backend));
        server
            .register_client(
                OAuthClient::builder("resource-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded client"),
            )
            .expect("register client");
        let (code, _) = server
            .authorize(&approved_resource_request("resource-client"))
            .expect("approved authorization");
        let code_digest = authorization_code_digest(&code);
        let cleanup_canary =
            insert_expired_authorization_code_cleanup_canary(&server, "resource-client");
        let before = server.stats();

        let error = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code,
                WRONG_RESOURCE,
            ))
            .expect_err("only the requested resource differs");

        assert!(matches!(error, OAuthError::InvalidGrant(_)));
        assert_oauth_stats_unchanged(&before, &server.stats());
        assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
        let state = server.state.read().expect("state");
        assert!(state.authorization_codes.contains_key(&cleanup_canary));
        assert_eq!(
            state
                .authorization_codes
                .get(&code_digest)
                .and_then(|code| code.resource.as_deref()),
            Some(RESOURCE)
        );
        drop(state);

        let issued = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code,
                RESOURCE,
            ))
            .expect("unchanged code remains exchangeable with its exact resource");
        assert_eq!(
            server
                .validate_access_token(&issued.access_token)
                .and_then(|token| token.resource),
            Some(RESOURCE.to_string())
        );
    }

    #[test]
    fn authorization_approval_binding_generation_scope_and_resource_mismatches_do_not_mutate() {
        for mode in [
            ApprovalTestMode::WrongBinding,
            ApprovalTestMode::WrongGeneration,
            ApprovalTestMode::WrongScopes,
            ApprovalTestMode::WrongResource,
        ] {
            let backend = Arc::new(CountingApprovalBackend::new(mode));
            let server = server_with_counting_approval(Arc::clone(&backend));
            server
                .register_client(
                    OAuthClient::builder("approval-client")
                        .redirect_uri("http://127.0.0.1/callback")
                        .scope("read")
                        .build()
                        .expect("bounded client"),
                )
                .expect("register client");
            let before = server.stats();

            let error = server
                .authorize(&approved_resource_request("approval-client"))
                .expect_err("one changed approval fact must reject");

            assert!(matches!(error, OAuthError::AccessDenied(_)));
            assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
            assert_oauth_stats_unchanged(&before, &server.stats());
        }
    }

    #[test]
    fn authorization_approval_denial_error_and_cancellation_do_not_create_codes() {
        for mode in [
            ApprovalTestMode::Denied,
            ApprovalTestMode::Error,
            ApprovalTestMode::Cancelled,
        ] {
            let backend = Arc::new(CountingApprovalBackend::new(mode));
            let server = server_with_counting_approval(Arc::clone(&backend));
            server
                .register_client(
                    OAuthClient::builder("approval-client")
                        .redirect_uri("http://127.0.0.1/callback")
                        .scope("read")
                        .build()
                        .expect("bounded client"),
                )
                .expect("register client");
            let before = server.stats();

            let error = server
                .authorize(&approved_resource_request("approval-client"))
                .expect_err("non-approved disposition must reject");

            assert!(matches!(
                error,
                OAuthError::AccessDenied(_) | OAuthError::TemporarilyUnavailable(_)
            ));
            assert_eq!(backend.calls.load(Ordering::SeqCst), 1);
            assert_oauth_stats_unchanged(&before, &server.stats());
        }
    }

    #[test]
    fn default_oauth_server_construction_is_fail_closed_without_an_approval_backend() {
        let server = OAuthServer::new(OAuthServerConfig::default());
        server
            .register_client(
                OAuthClient::builder("approval-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded client"),
            )
            .expect("register client");
        let before = server.stats();

        let error = server
            .authorize(&approved_resource_request("approval-client"))
            .expect_err("default construction must not silently approve");

        assert!(matches!(error, OAuthError::AccessDenied(_)));
        assert_oauth_stats_unchanged(&before, &server.stats());
    }

    #[test]
    fn oauth_token_verifier_resource_isolation_and_session_binding_rejection() {
        const RESOURCE_A: &str = "https://resource-a.example/api";
        const RESOURCE_B: &str = "https://resource-b.example/api";

        let backend = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let server = Arc::new(server_with_counting_approval(Arc::clone(&backend)));
        server
            .register_client(
                OAuthClient::builder("resource-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("bounded client"),
            )
            .expect("register client");

        // 1. Authorize and exchange code for RESOURCE_A
        let auth_req_a = AuthorizationRequest {
            resource: Some(RESOURCE_A.to_string()),
            state: Some("state-a".to_string()),
            scopes: vec!["read".to_string()],
            ..bounded_authorization_request("resource-client")
        };
        let (code_a, _) = server.authorize(&auth_req_a).expect("authorized a");
        let issued_a = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code_a,
                RESOURCE_A,
            ))
            .expect("exchange a");
        let refresh_a = issued_a.refresh_token.clone().expect("refresh token a");

        // Verify token A with OAuthTokenVerifier
        let cx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let verifier = server.token_verifier(configured_resource(RESOURCE_A));
        let auth_a = verifier
            .verify(
                &cx,
                AuthRequest {
                    method: "tools/call",
                    params: None,
                    transport_authorization: None,
                    request_id: 1,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: issued_a.access_token.clone(),
                },
            )
            .expect("verify token a");

        assert_eq!(auth_a.subject.as_deref(), Some("approved-subject"));
        assert_eq!(
            auth_a.claims.as_ref().and_then(|c| c["resource"].as_str()),
            Some(RESOURCE_A)
        );
        let owner_a = auth_a.session_owner().expect("session owner a");

        // 2. Refresh token A and verify rotated token retains same session_owner
        let rotated_a = server
            .token(&bounded_refresh_request("resource-client", &refresh_a))
            .expect("refresh rotation a");
        let rotated_auth_a = verifier
            .verify(
                &cx,
                AuthRequest {
                    method: "tools/call",
                    params: None,
                    transport_authorization: None,
                    request_id: 2,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: rotated_a.access_token,
                },
            )
            .expect("verify rotated token a");

        assert_eq!(rotated_auth_a.session_owner(), Some(owner_a));

        // 3. Authorize and exchange code for RESOURCE_B (same client, same subject, different resource)
        let auth_req_b = AuthorizationRequest {
            resource: Some(RESOURCE_B.to_string()),
            state: Some("state-b".to_string()),
            scopes: vec!["read".to_string()],
            ..bounded_authorization_request("resource-client")
        };
        let (code_b, _) = server.authorize(&auth_req_b).expect("authorized b");
        let issued_b = server
            .token(&resource_code_exchange_request(
                "resource-client",
                &code_b,
                RESOURCE_B,
            ))
            .expect("exchange b");

        let verifier_b = server.token_verifier(configured_resource(RESOURCE_B));
        let auth_b = verifier_b
            .verify(
                &cx,
                AuthRequest {
                    method: "tools/call",
                    params: None,
                    transport_authorization: None,
                    request_id: 3,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: issued_b.access_token,
                },
            )
            .expect("verify token b");

        assert_eq!(auth_b.subject.as_deref(), Some("approved-subject"));
        assert_eq!(
            auth_b.claims.as_ref().and_then(|c| c["resource"].as_str()),
            Some(RESOURCE_B)
        );
        let owner_b = auth_b.session_owner().expect("session owner b");

        // Different resources produce distinct session owners despite same client and subject
        assert_ne!(owner_a, owner_b);

        // 4. Also authorize and exchange for a token without resource
        let auth_req_no_res = AuthorizationRequest {
            resource: None,
            state: Some("state-no-res".to_string()),
            scopes: vec!["read".to_string()],
            ..bounded_authorization_request("resource-client")
        };
        let (code_no_res, _) = server
            .authorize(&auth_req_no_res)
            .expect("authorized no res");
        let issued_no_res = server
            .token(&bounded_code_exchange_request(
                "resource-client",
                &code_no_res,
            ))
            .expect("exchange no res");
        let unbound_metadata = server
            .validate_stored_access_token(&issued_no_res.access_token)
            .expect("unbound token remains valid authorization-server metadata");
        let unbound_error = verifier
            .verify(
                &cx,
                AuthRequest {
                    method: "tools/call",
                    params: None,
                    transport_authorization: None,
                    request_id: 4,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: issued_no_res.access_token,
                },
            )
            .expect_err("an unbound token cannot authenticate the protected resource");
        assert_eq!(unbound_error.code, McpErrorCode::ResourceForbidden);

        // Preserve the owner-namespace isolation check for retained unbound
        // metadata independently of admission, which now rejects that token.
        let auth_no_res = AuthContext::with_subject("approved-subject").with_session_owner(
            oauth_session_owner(
                &server.config.issuer,
                &unbound_metadata.client_id,
                unbound_metadata.registration_epoch,
                unbound_metadata.subject.as_deref(),
                None,
            )
            .expect("unbound metadata has a distinct owner namespace"),
        );

        let owner_no_res = auth_no_res.session_owner().expect("session owner no res");
        assert_ne!(owner_a, owner_no_res);
        assert_ne!(owner_b, owner_no_res);

        // 5. Connect to SessionPrincipalBinding consumer
        let fp_a = crate::auth::principal_fingerprint(Some(&auth_a)).expect("fingerprint a");
        let fp_a_rotated = crate::auth::principal_fingerprint(Some(&rotated_auth_a))
            .expect("fingerprint a rotated");
        let fp_b = crate::auth::principal_fingerprint(Some(&auth_b)).expect("fingerprint b");
        let fp_no_res =
            crate::auth::principal_fingerprint(Some(&auth_no_res)).expect("fingerprint no res");

        assert_eq!(fp_a, fp_a_rotated);
        assert_ne!(fp_a, fp_b);
        assert_ne!(fp_a, fp_no_res);
        assert_ne!(fp_b, fp_no_res);

        let binding = crate::session::SessionPrincipalBinding::default();
        // First frame with resource A binds the session
        assert!(binding.bind_or_verify(fp_a));
        // Same-resource refreshed token is accepted on the bound session
        assert!(binding.bind_or_verify(fp_a_rotated));
        // Different-resource token is rejected on the bound session
        assert!(!binding.bind_or_verify(fp_b));
        // Token without resource is rejected on the bound session
        assert!(!binding.bind_or_verify(fp_no_res));
        // Control-frame verification: existing bound principal matches
        assert!(binding.verify_existing(fp_a));
        assert!(binding.verify_existing(fp_a_rotated));
        assert!(!binding.verify_existing(fp_b));
        assert!(!binding.verify_existing(fp_no_res));
        // Original binding remains uncorrupted after cross-resource rejections
        assert!(binding.bind_or_verify(fp_a));
    }

    struct AudienceAuthTool {
        calls: Arc<AtomicUsize>,
    }

    impl crate::ToolHandler for AudienceAuthTool {
        fn definition(&self) -> fastmcp_protocol::Tool {
            fastmcp_protocol::Tool {
                name: "audience_auth".to_string(),
                description: Some("Returns admitted authentication facts".to_string()),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: None,
                icon: None,
                version: None,
                tags: Vec::new(),
                annotations: None,
            }
        }

        fn call(
            &self,
            ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<fastmcp_protocol::Content>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let auth = ctx
                .auth()
                .expect("authenticated handler must receive facts");
            Ok(vec![fastmcp_protocol::Content::text(
                serde_json::to_string(&auth).expect("safe auth facts serialize"),
            )])
        }
    }

    struct AudienceMiddleware {
        events: Arc<std::sync::Mutex<Vec<&'static str>>>,
    }

    impl crate::middleware::Middleware for AudienceMiddleware {
        fn on_request(
            &self,
            _ctx: &McpContext,
            _request: &fastmcp_protocol::JsonRpcRequest,
        ) -> McpResult<crate::middleware::MiddlewareDecision> {
            self.events.lock().unwrap().push("request");
            Ok(crate::middleware::MiddlewareDecision::Continue)
        }

        fn on_response(
            &self,
            _ctx: &McpContext,
            _request: &fastmcp_protocol::JsonRpcRequest,
            response: serde_json::Value,
        ) -> McpResult<serde_json::Value> {
            self.events.lock().unwrap().push("response");
            Ok(response)
        }

        fn on_error(
            &self,
            _ctx: &McpContext,
            _request: &fastmcp_protocol::JsonRpcRequest,
            error: McpError,
        ) -> McpError {
            self.events.lock().unwrap().push("error");
            error
        }
    }

    fn audience_http_request(token: &str, request_id: i64) -> fastmcp_transport::HttpRequest {
        let request = fastmcp_protocol::JsonRpcRequest::new(
            "tools/call",
            Some(serde_json::json!({
                "name": "audience_auth",
                "arguments": {},
                "_meta": {
                    "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                    "io.modelcontextprotocol/clientCapabilities": {},
                },
            })),
            request_id,
        );
        fastmcp_transport::HttpRequest::new(fastmcp_transport::HttpMethod::Post, "/api")
            .with_header("content-type", "application/json")
            .with_header("accept", "application/json")
            .with_header("mcp-protocol-version", "2026-07-28")
            .with_header("mcp-method", "tools/call")
            .with_header("mcp-name", "audience_auth")
            .with_header("authorization", format!("Bearer {token}"))
            .with_body(serde_json::to_vec(&request).expect("valid public HTTP request"))
    }

    fn assert_retained_token_unchanged(before: &StoredOAuthToken, after: &StoredOAuthToken) {
        assert_eq!(after.token, before.token);
        assert_eq!(after.token_type, before.token_type);
        assert_eq!(after.client_id, before.client_id);
        assert_eq!(after.scopes, before.scopes);
        assert_eq!(after.resource, before.resource);
        assert_eq!(after.issued_at, before.issued_at);
        assert_eq!(after.expires_at, before.expires_at);
        assert_eq!(after.subject, before.subject);
        assert_eq!(after.is_refresh_token, before.is_refresh_token);
        assert_eq!(after.grant_id, before.grant_id);
        assert_eq!(after.registration_epoch, before.registration_epoch);
        assert_eq!(after.family_expires_at, before.family_expires_at);
    }

    fn assert_fresh_http_resource_admission(token_resource: Option<&str>) {
        let approval = Arc::new(CountingApprovalBackend::new(ApprovalTestMode::Exact));
        let oauth = Arc::new(server_with_counting_approval(Arc::clone(&approval)));
        oauth
            .register_client(
                OAuthClient::builder("audience-client")
                    .redirect_uri("http://127.0.0.1/callback")
                    .scope("read")
                    .build()
                    .expect("valid client"),
            )
            .expect("client registered");
        let issue = |resource: Option<&str>| {
            let request = AuthorizationRequest {
                scopes: vec!["read".to_string()],
                resource: resource.map(str::to_string),
                ..bounded_authorization_request("audience-client")
            };
            let (code, _) = oauth.authorize(&request).expect("explicit approval");
            let exchange = TokenRequest {
                resource: resource.map(str::to_string),
                ..bounded_code_exchange_request("audience-client", &code)
            };
            oauth.token(&exchange).expect("actual PKCE token exchange")
        };
        let issued = issue(token_resource);
        let matching = issue(Some(TEST_MCP_RESOURCE));
        assert_eq!(approval.calls.load(Ordering::SeqCst), 2);
        let before = oauth
            .validate_stored_access_token(&issued.access_token)
            .expect("the presented token is valid at its issuer");
        let refresh_digest = refresh_token_digest(issued.refresh_token.as_ref().unwrap());
        let before_refresh = oauth.state.read().unwrap().refresh_tokens[&refresh_digest].clone();
        let before_stats = oauth.stats();
        let calls = Arc::new(AtomicUsize::new(0));
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let endpoint = crate::Server::new("audience-admission", "1.0.0")
            .protocol_policy(crate::ProtocolPolicy::ModernOnly)
            .expect("modern policy")
            .http_config(crate::HttpServerConfig::new().mcp_path("/api"))
            .auth_provider(crate::TokenAuthProvider::new(
                oauth.token_verifier(configured_resource(TEST_MCP_RESOURCE)),
            ))
            .tool(AudienceAuthTool {
                calls: Arc::clone(&calls),
            })
            .middleware(AudienceMiddleware {
                events: Arc::clone(&events),
            })
            // This test build also carries an unused exact-legacy endpoint,
            // whose POST sink accepts only a plain-HTTP origin. The modern
            // protected resource above is trusted HTTPS deployment policy;
            // this direct handler test does not model its TLS connection.
            .build_http_endpoint("http://resource.example")
            .expect("real public HTTP endpoint");
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().expect("test reactor"))
            .blocking_threads(2, 8)
            .build()
            .expect("caller-owned runtime");
        runtime.block_on(async {
            let cx = asupersync::Cx::current().expect("runtime context");
            let mut session = endpoint
                .open_session(&cx)
                .expect("fresh public HTTP session");
            let response = asupersync::time::timeout(
                cx.now(),
                Duration::from_secs(5),
                Box::pin(session.handle_async(&cx, audience_http_request(&issued.access_token, 1))),
            )
            .await
            .expect("HTTP admission must finish within the test deadline")
            .expect("HTTP admission result");
            let crate::ServerHttpEndpointResponse::Immediate(response) = response else {
                panic!("JSON request must produce one immediate HTTP response");
            };
            if token_resource == Some(TEST_MCP_RESOURCE) {
                assert_audience_http_success(&response, &issued.access_token, 1);
                assert_eq!(calls.load(Ordering::SeqCst), 1);
                assert_eq!(*events.lock().unwrap(), ["request", "response"]);
            } else {
                assert_eq!(response.status, fastmcp_transport::HttpStatus::UNAUTHORIZED);
                assert_eq!(
                    response.headers.get("www-authenticate").map(String::as_str),
                    Some("Bearer"),
                );
                assert!(response.body.is_empty());
                assert_eq!(calls.load(Ordering::SeqCst), 0);
                assert!(events.lock().unwrap().is_empty());
            }
            assert_oauth_stats_unchanged(&before_stats, &oauth.stats());
            assert_retained_token_unchanged(
                &before,
                &oauth
                    .validate_stored_access_token(&issued.access_token)
                    .expect("denial does not revoke token"),
            );
            assert_retained_token_unchanged(
                &before_refresh,
                &oauth.state.read().unwrap().refresh_tokens[&refresh_digest],
            );
            if let Some(resource) = token_resource {
                // This fixture's separately configured matching consumer can
                // still verify the same token after the HTTP audience check.
                let auth = oauth
                    .token_verifier(configured_resource(resource))
                    .verify(
                        &McpContext::new(cx.clone(), 3),
                        AuthRequest {
                            method: "tools/call",
                            params: None,
                            transport_authorization: None,
                            request_id: 3,
                        },
                        &AccessToken {
                            scheme: "Bearer".to_string(),
                            token: issued.access_token.clone(),
                        },
                    )
                    .expect("the token remains usable by its matching resource");
                assert_eq!(auth.claims.as_ref().unwrap()["resource"], resource);
            }

            // The same session remains usable by an audience-matching token.
            // In the negative this is its first admitted principal.
            let recovered = asupersync::time::timeout(
                cx.now(),
                Duration::from_secs(5),
                Box::pin(
                    session.handle_async(&cx, audience_http_request(&matching.access_token, 2)),
                ),
            )
            .await
            .expect("matching request must finish within the test deadline")
            .expect("matching request after the initial result");
            let crate::ServerHttpEndpointResponse::Immediate(recovered) = recovered else {
                panic!("matching JSON response must be immediate");
            };
            assert_audience_http_success(&recovered, &matching.access_token, 2);
            let expected_calls = if token_resource == Some(TEST_MCP_RESOURCE) {
                2
            } else {
                1
            };
            assert_eq!(calls.load(Ordering::SeqCst), expected_calls);
            assert_eq!(events.lock().unwrap().len(), expected_calls * 2);
            assert_oauth_stats_unchanged(&before_stats, &oauth.stats());
            session.close(&cx).await;
        });
    }

    fn assert_audience_http_success(
        response: &fastmcp_transport::HttpResponse,
        raw_token: &str,
        request_id: i64,
    ) {
        assert_eq!(response.status, fastmcp_transport::HttpStatus::OK);
        let rpc: fastmcp_protocol::JsonRpcResponse =
            serde_json::from_slice(&response.body).expect("complete JSON-RPC response");
        assert_eq!(rpc.id, Some(fastmcp_protocol::RequestId::from(request_id)));
        assert!(rpc.error.is_none());
        let result = rpc.result.expect("tool result");
        assert_eq!(
            result["content"].as_array().expect("content array").len(),
            1
        );
        let text = result["content"][0]["text"]
            .as_str()
            .expect("auth JSON text");
        let auth: serde_json::Value = serde_json::from_str(text).expect("safe handler auth facts");
        assert_eq!(auth["subject"], "approved-subject");
        assert_eq!(auth["scopes"], serde_json::json!(["read"]));
        assert_eq!(auth["claims"]["resource"], TEST_MCP_RESOURCE);
        assert_eq!(auth["claims"]["client_id"], "audience-client");
        assert!(!text.contains(raw_token));
        assert!(auth.get("token").is_none());
    }

    #[test]
    fn auth_01_a_matching_resource_dispatches_fresh_http() {
        assert_fresh_http_resource_admission(Some(TEST_MCP_RESOURCE));
    }

    #[test]
    fn auth_01_a_foreign_resource_rejects_before_fresh_http_dispatch() {
        assert_fresh_http_resource_admission(Some("https://foreign.example/api"));
    }

    #[test]
    fn auth_01_a_unbound_resource_rejects_before_fresh_http_dispatch() {
        assert_fresh_http_resource_admission(None);
    }

    fn bounded_refresh_request(client_id: &str, refresh_token: &str) -> TokenRequest {
        TokenRequest {
            grant_type: "refresh_token".to_string(),
            code: None,
            redirect_uri: None,
            client_id: client_id.to_string(),
            client_secret: None,
            code_verifier: None,
            refresh_token: Some(refresh_token.to_string()),
            scopes: None,
            resource: None,
        }
    }

    fn bounded_code_exchange_request(client_id: &str, code: &str) -> TokenRequest {
        TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some(code.to_string()),
            redirect_uri: Some("http://127.0.0.1/callback".to_string()),
            client_id: client_id.to_string(),
            client_secret: None,
            code_verifier: Some("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        }
    }

    #[test]
    fn test_client_builder() {
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .scope("read")
            .scope("write")
            .name("Test Client")
            .build()
            .unwrap();

        assert_eq!(client.client_id, "test-client");
        assert_eq!(client.client_type, ClientType::Public);
        assert_eq!(client.redirect_uris.len(), 1);
        assert!(client.allowed_scopes.contains("read"));
        assert!(client.allowed_scopes.contains("write"));
    }

    #[test]
    fn client_retention_exact_maxima_build_and_register() {
        let client_id = exact_ascii_value("client-", MAX_OAUTH_CLIENT_ID_BYTES);
        let credential = exact_ascii_value("credential-", MAX_OAUTH_CLIENT_CREDENTIAL_BYTES);
        let redirect_uris: Vec<_> = (0..MAX_OAUTH_REDIRECT_URIS_PER_CLIENT)
            .map(|index| {
                exact_ascii_value(
                    &format!("https://example.com/callback/{index}/"),
                    MAX_OAUTH_REDIRECT_URI_BYTES,
                )
            })
            .collect();
        let scopes: Vec<_> = (0..MAX_OAUTH_SCOPES_PER_CLIENT)
            .map(|index| exact_ascii_value(&format!("scope-{index}-"), MAX_OAUTH_SCOPE_BYTES))
            .collect();
        let name = exact_ascii_value("name-", MAX_OAUTH_CLIENT_NAME_BYTES);
        let description = exact_ascii_value("description-", MAX_OAUTH_CLIENT_DESCRIPTION_BYTES);

        let client = OAuthClient::builder(client_id.clone())
            .secret(credential)
            .redirect_uris(redirect_uris)
            .scopes(scopes)
            .name(name)
            .description(description)
            .build()
            .expect("every exact retention boundary is admitted");
        let server = OAuthServer::with_defaults();
        server
            .register_client(client)
            .expect("registration revalidation admits exact boundaries");

        let state = server.state.read().unwrap();
        let retained = state.clients.get(&client_id).expect("retained client");
        assert_eq!(retained.metadata.client_id.len(), MAX_OAUTH_CLIENT_ID_BYTES);
        assert!(retained.secret_verifier.is_some());
        assert_eq!(
            retained.metadata.redirect_uris.len(),
            MAX_OAUTH_REDIRECT_URIS_PER_CLIENT
        );
        assert!(
            retained
                .metadata
                .redirect_uris
                .iter()
                .all(|uri| uri.len() == MAX_OAUTH_REDIRECT_URI_BYTES)
        );
        assert_eq!(
            retained.metadata.allowed_scopes.len(),
            MAX_OAUTH_SCOPES_PER_CLIENT
        );
        assert!(
            retained
                .metadata
                .allowed_scopes
                .iter()
                .all(|scope| scope.len() == MAX_OAUTH_SCOPE_BYTES)
        );
        assert_eq!(
            retained.metadata.name.as_deref().map(str::len),
            Some(MAX_OAUTH_CLIENT_NAME_BYTES)
        );
        assert_eq!(
            retained.metadata.description.as_deref().map(str::len),
            Some(MAX_OAUTH_CLIENT_DESCRIPTION_BYTES)
        );
    }

    #[test]
    fn client_authentication_accepts_exact_bound_and_rejects_one_past() {
        let credential = exact_ascii_value("credential-", MAX_OAUTH_CLIENT_CREDENTIAL_BYTES);
        let client = OAuthClient::builder("confidential")
            .secret(credential.clone())
            .redirect_uri("https://example.com/callback")
            .build()
            .unwrap();

        assert!(client.authenticate(Some(&credential)));

        let mut same_length_wrong_credential = credential.clone();
        same_length_wrong_credential.pop();
        same_length_wrong_credential.push('y');
        assert!(!client.authenticate(Some(&same_length_wrong_credential)));

        let one_past_credential = "x".repeat(MAX_OAUTH_CLIENT_CREDENTIAL_BYTES + 1);
        assert!(!client.authenticate(Some(&one_past_credential)));
        assert!(!client.authenticate(None));
    }

    #[test]
    fn client_builder_rejects_one_past_every_retention_bound() {
        assert_client_build_error(
            OAuthClient::builder("x".repeat(MAX_OAUTH_CLIENT_ID_BYTES + 1))
                .redirect_uri("https://example.com/callback")
                .build(),
            OAUTH_CLIENT_ID_RETENTION_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .secret("x".repeat(MAX_OAUTH_CLIENT_CREDENTIAL_BYTES + 1))
                .redirect_uri("https://example.com/callback")
                .build(),
            OAUTH_CLIENT_CREDENTIAL_RETENTION_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uris(
                    (0..=MAX_OAUTH_REDIRECT_URIS_PER_CLIENT)
                        .map(|index| format!("https://example.com/{index}")),
                )
                .build(),
            OAUTH_CLIENT_REDIRECT_COUNT_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri(exact_ascii_value(
                    "https://example.com/",
                    MAX_OAUTH_REDIRECT_URI_BYTES + 1,
                ))
                .build(),
            OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri("https://example.com/callback")
                .scopes((0..=MAX_OAUTH_SCOPES_PER_CLIENT).map(|index| format!("scope-{index}")))
                .build(),
            OAUTH_CLIENT_SCOPE_COUNT_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri("https://example.com/callback")
                .scope("x".repeat(MAX_OAUTH_SCOPE_BYTES + 1))
                .build(),
            OAUTH_CLIENT_SCOPE_VALUE_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri("https://example.com/callback")
                .name("x".repeat(MAX_OAUTH_CLIENT_NAME_BYTES + 1))
                .build(),
            OAUTH_CLIENT_NAME_RETENTION_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri("https://example.com/callback")
                .description("x".repeat(MAX_OAUTH_CLIENT_DESCRIPTION_BYTES + 1))
                .build(),
            OAUTH_CLIENT_DESCRIPTION_RETENTION_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .secret("")
                .redirect_uri("https://example.com/callback")
                .build(),
            OAUTH_CLIENT_CREDENTIAL_RETENTION_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client").redirect_uri("").build(),
            OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
        );
        assert_client_build_error(
            OAuthClient::builder("client")
                .redirect_uri("https://example.com/callback")
                .scope("")
                .build(),
            OAUTH_CLIENT_SCOPE_VALUE_ERROR,
        );
    }

    #[test]
    fn client_builder_rejects_control_and_bidi_display_metadata() {
        for unsafe_value in [
            "line\nbreak",
            "right-to-left\u{202e}override",
            "isolated\u{2067}segment\u{2069}",
            "paragraph\u{2029}break",
        ] {
            assert_client_build_error(
                OAuthClient::builder("client")
                    .redirect_uri("https://example.com/callback")
                    .name(unsafe_value)
                    .build(),
                OAUTH_CLIENT_NAME_RETENTION_ERROR,
            );
            assert_client_build_error(
                OAuthClient::builder("client")
                    .redirect_uri("https://example.com/callback")
                    .description(unsafe_value)
                    .build(),
                OAUTH_CLIENT_DESCRIPTION_RETENTION_ERROR,
            );
        }
    }

    #[test]
    fn client_builder_rejects_reserved_authorization_response_query_parameters() {
        for query in [
            "code=attacker",
            "state=attacker",
            "error=attacker",
            "error_description=attacker",
            "error_uri=https%3A%2F%2Fattacker.example",
            "iss=https%3A%2F%2Fattacker.example",
            "c%6fde=percent-encoded-name",
            "safe=value&state=attacker",
            "safe=value;error=attacker",
        ] {
            assert_client_build_error(
                OAuthClient::builder("client")
                    .redirect_uri(format!("https://example.com/callback?{query}"))
                    .build(),
                OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
            );
        }

        OAuthClient::builder("client")
            .redirect_uri("https://example.com/callback?safe=value")
            .build()
            .expect("unrelated redirect query parameters remain valid");
    }

    #[test]
    fn registration_revalidates_every_publicly_mutable_client_bound() {
        assert_registration_rejects_mutation(
            |client| client.client_id = "x".repeat(MAX_OAUTH_CLIENT_ID_BYTES + 1),
            OAUTH_CLIENT_ID_RETENTION_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.client_secret = Some(ClientSecret::new(
                    "x".repeat(MAX_OAUTH_CLIENT_CREDENTIAL_BYTES + 1),
                ));
                client.client_type = ClientType::Confidential;
            },
            OAUTH_CLIENT_CREDENTIAL_RETENTION_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.redirect_uris = vec![
                    "https://example.com/callback".to_string();
                    MAX_OAUTH_REDIRECT_URIS_PER_CLIENT + 1
                ];
            },
            OAUTH_CLIENT_REDIRECT_COUNT_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.redirect_uris = vec![exact_ascii_value(
                    "https://example.com/",
                    MAX_OAUTH_REDIRECT_URI_BYTES + 1,
                )];
            },
            OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.allowed_scopes = (0..=MAX_OAUTH_SCOPES_PER_CLIENT)
                    .map(|index| format!("scope-{index}"))
                    .collect();
            },
            OAUTH_CLIENT_SCOPE_COUNT_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.allowed_scopes = HashSet::from(["x".repeat(MAX_OAUTH_SCOPE_BYTES + 1)]);
            },
            OAUTH_CLIENT_SCOPE_VALUE_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| client.name = Some("x".repeat(MAX_OAUTH_CLIENT_NAME_BYTES + 1)),
            OAUTH_CLIENT_NAME_RETENTION_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| client.name = Some("spoof\u{202e}name".to_string()),
            OAUTH_CLIENT_NAME_RETENTION_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.description = Some("x".repeat(MAX_OAUTH_CLIENT_DESCRIPTION_BYTES + 1));
            },
            OAUTH_CLIENT_DESCRIPTION_RETENTION_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| client.client_type = ClientType::Confidential,
            OAUTH_CLIENT_CREDENTIAL_CLASS_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| client.redirect_uris = vec!["javascript:alert(1)".to_string()],
            OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| {
                client.redirect_uris =
                    vec!["https://example.com/callback?code=attacker".to_string()];
            },
            OAUTH_CLIENT_REDIRECT_VALUE_ERROR,
        );
        assert_registration_rejects_mutation(
            |client| client.allowed_scopes = HashSet::from(["read write".to_string()]),
            OAUTH_CLIENT_SCOPE_VALUE_ERROR,
        );
    }

    #[test]
    fn authorization_retention_accepts_exact_bounds() {
        let scopes: Vec<_> = (0..MAX_OAUTH_SCOPES_PER_CLIENT)
            .map(|index| exact_ascii_value(&format!("scope-{index}-"), MAX_OAUTH_SCOPE_BYTES))
            .collect();
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("bounded")
            .redirect_uri("http://127.0.0.1/callback")
            .scopes(scopes.clone())
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let mut request = bounded_authorization_request("bounded");
        request.scopes = scopes;
        request.state = Some("s".repeat(MAX_OAUTH_STATE_BYTES));
        let (code, _) = server.authorize(&request).unwrap();

        let state = server.state.read().unwrap();
        let retained = state
            .authorization_codes
            .get(&authorization_code_digest(&code))
            .unwrap();
        assert_eq!(retained.scopes.len(), MAX_OAUTH_SCOPES_PER_CLIENT);
        assert_eq!(
            retained.state.as_ref().map(String::len),
            Some(MAX_OAUTH_STATE_BYTES)
        );
        assert_eq!(
            retained.subject.as_ref().map(String::len),
            Some("oauth-test-subject".len())
        );
    }

    #[test]
    fn authorization_scope_duplicates_are_canonicalized_before_retention() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("bounded")
            .redirect_uri("http://127.0.0.1/callback")
            .scope("read")
            .scope("write")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let mut request = bounded_authorization_request("bounded");
        request.scopes = vec!["read".to_string(), "read".to_string(), "write".to_string()];
        let (code, _) = server.authorize(&request).unwrap();

        let state = server.state.read().unwrap();
        assert_eq!(
            state
                .authorization_codes
                .get(&authorization_code_digest(&code))
                .unwrap()
                .scopes,
            vec!["read".to_string(), "write".to_string()]
        );
    }

    #[test]
    fn authorization_retention_rejects_one_past_before_token_draw() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("bounded")
            .redirect_uri("http://127.0.0.1/callback")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        let draws = std::cell::Cell::new(0);

        let request = bounded_authorization_request("bounded");
        let error = server
            .authorize_with_token_draw(
                &AuthorizationRequest {
                    state: Some("s".repeat(MAX_OAUTH_STATE_BYTES + 1)),
                    ..request.clone()
                },
                || {
                    draws.set(draws.get() + 1);
                    draw_security_identifier().map_err(|_| "unexpected RNG failure")
                },
            )
            .unwrap_err();
        assert_eq!(
            error.description(),
            OAUTH_AUTHORIZATION_STATE_RETENTION_ERROR
        );
        assert_eq!(draws.get(), 0);

        let error = server
            .authorize_with_token_draw(
                &AuthorizationRequest {
                    scopes: vec!["x".repeat(MAX_OAUTH_SCOPE_BYTES + 1)],
                    ..request.clone()
                },
                || {
                    draws.set(draws.get() + 1);
                    draw_security_identifier().map_err(|_| "unexpected RNG failure")
                },
            )
            .unwrap_err();
        assert_eq!(error.description(), OAUTH_REQUEST_SCOPE_VALUE_ERROR);
        assert_eq!(draws.get(), 0);

        let error = server
            .authorize_with_token_draw(
                &AuthorizationRequest {
                    scopes: vec!["read".to_string(); MAX_OAUTH_SCOPES_PER_CLIENT + 1],
                    ..request
                },
                || {
                    draws.set(draws.get() + 1);
                    draw_security_identifier().map_err(|_| "unexpected RNG failure")
                },
            )
            .unwrap_err();
        assert_eq!(error.description(), OAUTH_REQUEST_SCOPE_COUNT_ERROR);
        assert_eq!(draws.get(), 0);
        assert!(server.state.read().unwrap().authorization_codes.is_empty());
    }

    #[test]
    fn test_confidential_client() {
        let client = OAuthClient::builder("test-client")
            .secret("super-secret")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();

        assert_eq!(client.client_type, ClientType::Confidential);
        assert!(client.authenticate(Some("super-secret")));
        assert!(!client.authenticate(Some("wrong-secret")));
        assert!(!client.authenticate(None));
    }

    #[test]
    fn test_redirect_uri_validation() {
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .redirect_uri("https://example.com/oauth/callback")
            .build()
            .unwrap();

        // Exact match
        assert!(client.validate_redirect_uri("http://127.0.0.1:3000/callback"));
        assert!(client.validate_redirect_uri("https://example.com/oauth/callback"));

        // The exact loopback IP may use a different ephemeral port.
        assert!(client.validate_redirect_uri("http://127.0.0.1:8080/callback"));

        // Invalid
        assert!(!client.validate_redirect_uri("http://127.0.0.1:3000/other"));
        assert!(!client.validate_redirect_uri("http://localhost:3000/callback"));
        assert!(!client.validate_redirect_uri("https://evil.com/callback"));
        assert!(!client.validate_redirect_uri("http://localhost:3000@evil.example/callback"));
        assert!(!client.validate_redirect_uri("https://example.com/oauth/callback#fragment"));

        // Even an exact registration cannot opt into an authority-confusion
        // URI containing userinfo.
        let mut confused = OAuthClient::builder("confused")
            .redirect_uri("http://127.0.0.1/callback")
            .build()
            .unwrap();
        confused.redirect_uris = vec!["http://localhost:3000@evil.example/callback".to_string()];
        assert!(!confused.validate_redirect_uri("http://localhost:3000@evil.example/callback"));

        // Registration cannot opt into fragments either; RFC 6749 forbids a
        // redirect endpoint from carrying a fragment component.
        let mut fragmented = OAuthClient::builder("fragmented")
            .redirect_uri("https://example.com/callback")
            .build()
            .unwrap();
        fragmented.redirect_uris =
            vec!["https://example.com/callback#registered-fragment".to_string()];
        assert!(
            !fragmented.validate_redirect_uri("https://example.com/callback#registered-fragment")
        );
    }

    #[test]
    fn redirect_registration_rejects_unsafe_urls_with_fixed_error() {
        for uri in [
            "http://localhost:3000/callback",
            "http://example.com/callback",
            "http://127.1/callback",
            "http://0x7f000001/callback",
            "http://[0:0:0:0:0:0:0:1]/callback",
            "javascript:alert(1)",
            "/relative/callback",
            "https://user:password@example.com/callback",
            "https://@example.com/callback",
            "https://example.com/callback#fragment",
            "https://example.com/callback\r\nheader",
        ] {
            let error = OAuthClient::builder("client")
                .redirect_uri(uri)
                .build()
                .expect_err("unsafe redirect URI must fail closed");
            assert_eq!(error.description(), OAUTH_CLIENT_REDIRECT_VALUE_ERROR);
            assert!(!error.description().contains(uri));
        }

        for uri in [
            "https://example.com/callback",
            "http://127.0.0.1:3000/callback",
            "http://[::1]:3000/callback",
        ] {
            assert!(
                OAuthClient::builder("client")
                    .redirect_uri(uri)
                    .build()
                    .is_ok()
            );
        }
    }

    #[test]
    fn loopback_redirect_exception_changes_only_the_port_bytes() {
        let client = OAuthClient::builder("native-client")
            .redirect_uri("http://127.0.0.1:3000/a/callback?resource=%2Fone&mode=x")
            .build()
            .unwrap();

        assert!(
            client
                .validate_redirect_uri("http://127.0.0.1:49152/a/callback?resource=%2Fone&mode=x")
        );
        assert!(!client.validate_redirect_uri(
            "http://127.0.0.1:49152/a/../a/callback?resource=%2Fone&mode=x"
        ));
        assert!(
            !client
                .validate_redirect_uri("http://127.0.0.1:49152/a/callback?resource=%2fone&mode=x")
        );
        assert!(
            !client
                .validate_redirect_uri("http://127.0.0.1:49152/a/callback?mode=x&resource=%2Fone")
        );
        assert!(
            !client
                .validate_redirect_uri("http://127.0.0.1:049152/a/callback?resource=%2Fone&mode=x")
        );
    }

    #[test]
    fn test_scope_validation() {
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .scope("read")
            .scope("write")
            .build()
            .unwrap();

        assert!(client.validate_scopes(&["read".to_string()]));
        assert!(client.validate_scopes(&["read".to_string(), "write".to_string()]));
        assert!(!client.validate_scopes(&["admin".to_string()]));
    }

    #[test]
    fn test_oauth_server_client_registration() {
        let server = OAuthServer::with_defaults();

        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();

        server.register_client(client).unwrap();

        // Duplicate registration should fail
        let client2 = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        assert!(server.register_client(client2).is_err());

        // Verify client exists
        assert!(server.get_client("test-client").is_some());
        assert!(server.get_client("nonexistent").is_none());
    }

    #[test]
    fn server_client_reads_return_secret_free_metadata() {
        const SECRET: &str = "metadata-must-not-clone-this-client-secret";
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("confidential-client")
            .secret(SECRET)
            .redirect_uri("https://example.com/callback")
            .scope("read")
            .name("Confidential Client")
            .description("metadata read model")
            .build()
            .unwrap();
        let registered_at = client.registered_at;
        server.register_client(client).unwrap();

        let metadata = server
            .get_client("confidential-client")
            .expect("registered client metadata");
        assert_eq!(metadata.client_id, "confidential-client");
        assert_eq!(metadata.client_type, ClientType::Confidential);
        assert_eq!(metadata.registered_at, registered_at);
        assert_eq!(metadata.redirect_uris, ["https://example.com/callback"]);
        assert!(metadata.allowed_scopes.contains("read"));
        assert_eq!(metadata.name.as_deref(), Some("Confidential Client"));
        assert_eq!(metadata.description.as_deref(), Some("metadata read model"));
        assert!(!format!("{metadata:?}").contains(SECRET));

        let listed = server.list_clients();
        assert_eq!(listed, [metadata]);
        assert!(!format!("{listed:?}").contains(SECRET));
    }

    #[test]
    fn registration_replaces_plaintext_client_secret_with_verifier() {
        const SECRET: &str = "registration-only-confidential-secret";
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("confidential-client")
            .secret(SECRET)
            .redirect_uri("https://example.com/callback")
            .build()
            .unwrap();
        assert!(client.has_client_secret());
        server.register_client(client).unwrap();

        let state = server.state.read().unwrap();
        let registered = state.clients.get("confidential-client").unwrap();
        let verifier = registered.secret_verifier.expect("stored verifier");
        assert!(verifier.verify(SECRET.as_bytes()));
        assert!(!verifier.verify(b"wrong-secret"));
        assert!(!format!("{verifier:?}").contains(SECRET));
        assert_eq!(registered.metadata.client_type, ClientType::Confidential);
    }

    #[test]
    fn public_client_rejects_supplied_secret_before_grant_access() {
        let server = OAuthServer::with_defaults();
        server
            .register_client(bounded_test_client("public"))
            .unwrap();
        let (code, _) = server
            .authorize(&bounded_authorization_request("public"))
            .unwrap();
        let mut exchange = bounded_code_exchange_request("public", &code);
        exchange.client_secret = Some("must-not-be-accepted".to_string());

        let exchange_error = server.token(&exchange).unwrap_err();
        assert_eq!(exchange_error.error_code(), "invalid_client");
        assert_eq!(
            exchange_error.description(),
            OAUTH_CLIENT_AUTHENTICATION_ERROR
        );
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );

        let issued = issue_access_token_via_auth_code(
            &server,
            "public",
            "http://127.0.0.1/callback",
            &[],
            "subject",
        );
        let refresh = issued.refresh_token.as_deref().unwrap();
        let mut refresh_request = bounded_refresh_request("public", refresh);
        refresh_request.client_secret = Some("must-not-be-accepted".to_string());
        let refresh_error = server.token(&refresh_request).unwrap_err();
        assert_eq!(refresh_error.error_code(), "invalid_client");
        assert_eq!(refresh_error.description(), exchange_error.description());

        let revoke_error = server
            .revoke(&issued.access_token, "public", Some("must-not-be-accepted"))
            .unwrap_err();
        assert_eq!(revoke_error.error_code(), "invalid_client");
        assert_eq!(revoke_error.description(), exchange_error.description());
        assert!(server.validate_access_token(&issued.access_token).is_some());
    }

    #[test]
    fn invalid_client_response_precedes_code_and_refresh_lookup() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("confidential")
            .secret("correct-secret")
            .redirect_uri("http://127.0.0.1/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let absent_credential = base64url_encode(&[0x77_u8; 32]);
        let mut code_request = bounded_code_exchange_request("confidential", &absent_credential);
        code_request.client_secret = Some("wrong-secret".to_string());
        let wrong_secret = server.token(&code_request).unwrap_err();

        code_request.client_id = "unknown-client".to_string();
        let unknown_client = server.token(&code_request).unwrap_err();
        assert_eq!(wrong_secret.error_code(), "invalid_client");
        assert_eq!(unknown_client.error_code(), "invalid_client");
        assert_eq!(wrong_secret.description(), unknown_client.description());
        assert_eq!(
            wrong_secret.description(),
            OAUTH_CLIENT_AUTHENTICATION_ERROR
        );

        let mut refresh_request = bounded_refresh_request("confidential", &absent_credential);
        refresh_request.client_secret = Some("wrong-secret".to_string());
        let refresh_error = server.token(&refresh_request).unwrap_err();
        assert_eq!(refresh_error.error_code(), "invalid_client");
        assert_eq!(refresh_error.description(), wrong_secret.description());
    }

    #[test]
    fn test_authorization_flow() {
        let server = OAuthServer::with_defaults();

        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Create authorization request
        let request = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "test-client".to_string(),
            redirect_uri: "http://127.0.0.1:3000/callback".to_string(),
            scopes: vec!["read".to_string()],
            resource: None,
            state: Some("xyz".to_string()),
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };

        let (code, redirect) = server.authorize(&request).unwrap();

        assert_ne!(code.len(), 0);
        assert!(redirect.contains("code="));
        assert!(redirect.contains("state=xyz"));
    }

    #[test]
    fn test_pkce_required() {
        let server = OAuthServer::with_defaults();

        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Request without PKCE should fail
        let request = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "test-client".to_string(),
            redirect_uri: "http://127.0.0.1:3000/callback".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: String::new(), // Missing!
            code_challenge_method: CodeChallengeMethod::S256,
        };

        let result = server.authorize(&request);
        assert!(matches!(result, Err(OAuthError::InvalidRequest(_))));
    }

    #[test]
    fn authorization_path_rejects_plain_and_malformed_s256_challenges() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let plain = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "test-client".to_string(),
            redirect_uri: "http://127.0.0.1:3000/callback".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: verifier.to_string(),
            code_challenge_method: CodeChallengeMethod::Plain,
        };
        let error = server.authorize(&plain).unwrap_err();
        assert!(matches!(error, OAuthError::InvalidRequest(_)));

        let malformed_s256 = AuthorizationRequest {
            code_challenge: "!".repeat(43),
            code_challenge_method: CodeChallengeMethod::S256,
            ..plain
        };
        let error = server.authorize(&malformed_s256).unwrap_err();
        assert!(matches!(error, OAuthError::InvalidRequest(_)));
        assert!(server.state.read().unwrap().authorization_codes.is_empty());
    }

    #[test]
    fn authorization_approval_decision_rejects_empty_subject() {
        let request = AuthorizationApprovalRequest {
            binding: AuthorizationApprovalBinding {
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/callback".to_string(),
                scopes: Vec::new(),
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(
                    "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                )
                .expect("valid challenge"),
                code_challenge_method: CodeChallengeMethod::S256,
                registration_epoch: test_registration_epoch(1),
            },
        };

        let error = request
            .approve(
                String::new(),
                Vec::new(),
                None,
                AuthorizationApprovalGeneration::from_bytes([1; 32]),
            )
            .expect_err("an empty subject is not a usable approval identity");

        assert!(matches!(error, OAuthError::InvalidRequest(_)));
    }

    #[test]
    fn direct_issuance_rejects_empty_subject_before_token_draw() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let draws = std::cell::Cell::new(0);

        let error = server
            .issue_tokens_with_draw("c1", &[], None, Some(""), || {
                draws.set(draws.get() + 1);
                draw_security_identifier().map_err(|_| "unexpected RNG failure")
            })
            .expect_err("an empty subject is not a usable owner identity");

        assert!(matches!(error, OAuthError::InvalidRequest(_)));
        assert_eq!(draws.get(), 0);
        let state = server.state.read().unwrap();
        assert!(state.access_tokens.is_empty());
        assert!(state.refresh_tokens.is_empty());
    }

    #[test]
    fn authorization_rejects_client_reregistration_during_code_draw() {
        let server = Arc::new(OAuthServer::with_defaults());
        server.register_client(bounded_test_client("c1")).unwrap();
        let request = bounded_authorization_request("c1");
        let (draw_started_tx, draw_started_rx) = std::sync::mpsc::sync_channel(0);
        let (resume_draw_tx, resume_draw_rx) = std::sync::mpsc::sync_channel(0);
        let authorizing_server = Arc::clone(&server);

        let authorizing = std::thread::spawn(move || {
            authorizing_server.authorize_with_token_draw(&request, || {
                draw_started_tx.send(()).expect("signal code draw");
                resume_draw_rx.recv().expect("resume code draw");
                draw_security_identifier().map_err(|_| "unexpected RNG failure")
            })
        });

        draw_started_rx.recv().expect("authorization reached draw");
        server.unregister_client("c1").unwrap();
        server.register_client(bounded_test_client("c1")).unwrap();
        resume_draw_tx.send(()).expect("resume authorization");
        let error = authorizing
            .join()
            .expect("authorization thread")
            .expect_err("old authorization must not transfer to a new registration");

        assert!(matches!(error, OAuthError::InvalidClient(_)));
        assert!(server.state.read().unwrap().authorization_codes.is_empty());
    }

    #[test]
    fn test_token_generation() {
        let value = generate_token().unwrap();

        assert_eq!(value.len(), 43);
        assert!(!value.contains('='));
        assert!(
            value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        );
    }

    #[test]
    fn opaque_credential_digests_are_domain_separated_and_redacted() {
        let value = base64url_encode(&[0x42_u8; 32]);
        let authorization_code = authorization_code_digest(&value);
        let access_token = access_token_digest(&value);
        let refresh_token = refresh_token_digest(&value);

        assert_ne!(authorization_code, access_token);
        assert_ne!(authorization_code, refresh_token);
        assert_ne!(access_token, refresh_token);
        assert!(!format!("{authorization_code:?}").contains(&value));
        assert!(validate_opaque_credential(&value, "invalid").is_ok());
        assert!(validate_opaque_credential(&format!("{value}="), "invalid").is_err());
        assert!(validate_opaque_credential(&value[..42], "invalid").is_err());
    }

    #[test]
    fn pkce_s256_matches_rfc_7636_and_enforces_fixed_input() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_s256_challenge(verifier).unwrap();
        assert_eq!(challenge, "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM");
        assert!(validate_s256_code_challenge(&challenge).is_ok());
        assert!(validate_s256_code_challenge("short").is_err());
        assert!(validate_s256_code_challenge(&"!".repeat(43)).is_err());
        let noncanonical = format!("{}N", &challenge[..42]);
        assert!(validate_s256_code_challenge(&noncanonical).is_err());

        assert!(compute_s256_challenge(&"A".repeat(43)).is_ok());
        assert!(compute_s256_challenge(&"~".repeat(128)).is_ok());
        assert!(compute_s256_challenge(&"A".repeat(42)).is_err());
        assert!(compute_s256_challenge(&"A".repeat(129)).is_err());
        assert!(compute_s256_challenge(&format!("{}%", "A".repeat(42))).is_err());
        assert!(compute_s256_challenge(&"é".repeat(43)).is_err());
    }

    #[test]
    fn authorization_draw_failure_precedes_code_storage() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let request = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "test-client".to_string(),
            redirect_uri: "http://127.0.0.1:3000/callback".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let draw_calls = std::cell::Cell::new(0);

        let result = server.authorize_with_token_draw(&request, || {
            draw_calls.set(draw_calls.get() + 1);
            Err::<SecurityIdentifier, _>("forced security-identifier draw failure")
        });

        assert!(matches!(result, Err(OAuthError::ServerError(_))));
        assert_eq!(draw_calls.get(), 1);
        assert!(server.state.read().unwrap().authorization_codes.is_empty());
    }

    #[test]
    fn authorization_lifetime_starts_after_credential_generation() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let final_draw_completed = std::cell::Cell::new(None);

        let (code, _) = server
            .authorize_with_token_draw(&bounded_authorization_request("c1"), || {
                let identifier = draw_security_identifier()
                    .map_err(|_| "unexpected operating-system RNG failure")?;
                final_draw_completed.set(Some(Instant::now()));
                Ok::<_, &str>(identifier)
            })
            .expect("authorization succeeds");

        let state = server.state.read().unwrap();
        let stored = state
            .authorization_codes
            .get(&authorization_code_digest(&code))
            .expect("stored authorization code");
        assert!(
            stored.issued_at
                >= final_draw_completed
                    .get()
                    .expect("draw completion timestamp")
        );
        assert_eq!(
            stored
                .expires_at
                .saturating_duration_since(stored.issued_at),
            server.config.authorization_code_lifetime
        );
    }

    #[test]
    fn second_token_draw_failure_commits_neither_token() {
        let server = OAuthServer::with_defaults();
        server
            .register_client(bounded_test_client("client"))
            .unwrap();
        let draw_calls = std::cell::Cell::new(0);

        let result = server.issue_tokens_with_draw("client", &[], None, None, || {
            let call = draw_calls.get() + 1;
            draw_calls.set(call);
            if call == 1 {
                draw_security_identifier().map_err(|_| "unexpected operating-system RNG failure")
            } else {
                Err("forced second security-identifier draw failure")
            }
        });

        assert!(matches!(result, Err(OAuthError::ServerError(_))));
        assert_eq!(draw_calls.get(), 2);
        let state = server.state.read().unwrap();
        assert!(state.access_tokens.is_empty());
        assert!(state.refresh_tokens.is_empty());
    }

    #[test]
    fn token_pair_consumes_two_fresh_security_identifier_draws() {
        let server = OAuthServer::with_defaults();
        server
            .register_client(bounded_test_client("client"))
            .unwrap();
        let draw_calls = std::cell::Cell::new(0);
        let final_draw_completed = std::cell::Cell::new(None);

        let response = server
            .issue_tokens_with_draw("client", &[], None, None, || {
                let call = draw_calls.get() + 1;
                draw_calls.set(call);
                let identifier = draw_security_identifier()
                    .map_err(|_| "unexpected operating-system RNG failure")?;
                if call == 2 {
                    final_draw_completed.set(Some(Instant::now()));
                }
                Ok::<_, &str>(identifier)
            })
            .unwrap();

        assert_eq!(draw_calls.get(), 2);
        assert_eq!(response.access_token.len(), 43);
        assert_eq!(response.refresh_token.as_deref().unwrap().len(), 43);
        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 1);
        assert_eq!(state.refresh_tokens.len(), 1);
        let access = state
            .access_tokens
            .get(&access_token_digest(&response.access_token))
            .expect("returned access token was committed");
        let refresh = state
            .refresh_tokens
            .get(&refresh_token_digest(
                response
                    .refresh_token
                    .as_deref()
                    .expect("token pair contains refresh token"),
            ))
            .expect("returned refresh token was committed");
        assert_eq!(access.token.len(), 0);
        assert_eq!(refresh.token.len(), 0);
        let final_draw_completed = final_draw_completed
            .get()
            .expect("second draw completion timestamp");
        assert!(access.metadata.issued_at >= final_draw_completed);
        assert!(refresh.metadata.issued_at >= final_draw_completed);
    }

    #[test]
    fn refresh_access_draw_failure_preserves_existing_token_state() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("client")
            .redirect_uri("http://127.0.0.1/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        let issued = server.issue_tokens("client", &[], Some("subject")).unwrap();
        let refresh_token = issued.refresh_token.unwrap();
        let request = TokenRequest {
            grant_type: "refresh_token".to_string(),
            code: None,
            redirect_uri: None,
            client_id: "client".to_string(),
            client_secret: None,
            code_verifier: None,
            refresh_token: Some(refresh_token.clone()),
            scopes: None,
            resource: None,
        };
        let draw_calls = std::cell::Cell::new(0);

        let result = server.token_refresh_token_with_draw(&request, || {
            draw_calls.set(draw_calls.get() + 1);
            Err::<SecurityIdentifier, _>("forced refresh access-token draw failure")
        });

        assert!(matches!(result, Err(OAuthError::ServerError(_))));
        assert_eq!(draw_calls.get(), 1);
        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 1);
        assert_eq!(state.refresh_tokens.len(), 1);
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&refresh_token))
        );
    }

    #[test]
    fn test_base64url_encode() {
        // Test vectors from RFC 4648
        assert_eq!(base64url_encode(b""), "");
        assert_eq!(base64url_encode(b"f"), "Zg");
        assert_eq!(base64url_encode(b"fo"), "Zm8");
        assert_eq!(base64url_encode(b"foo"), "Zm9v");
        assert_eq!(base64url_encode(b"foob"), "Zm9vYg");
        assert_eq!(base64url_encode(b"fooba"), "Zm9vYmE");
        assert_eq!(base64url_encode(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn test_url_encode() {
        assert_eq!(url_encode("hello"), "hello");
        assert_eq!(url_encode("hello world"), "hello%20world");
        assert_eq!(url_encode("a=b&c=d"), "a%3Db%26c%3Dd");
    }

    #[test]
    fn test_constant_time_eq() {
        assert!(constant_time_eq("hello", "hello"));
        assert!(!constant_time_eq("hello", "world"));
        assert!(!constant_time_eq("hello", "hell"));
    }

    #[test]
    fn test_loopback_match() {
        assert!(loopback_match(
            "http://127.0.0.1:3000/callback",
            "http://127.0.0.1:8080/callback"
        ));
        assert!(!loopback_match(
            "http://127.0.0.1:3000/callback",
            "http://localhost:8080/callback"
        ));
        assert!(!loopback_match(
            "http://127.0.0.1:3000/callback",
            "http://127.0.0.1:3000/other"
        ));
    }

    #[test]
    fn test_oauth_server_stats() {
        let server = OAuthServer::with_defaults();

        let stats = server.stats();
        assert_eq!(stats.clients, 0);
        assert_eq!(stats.access_tokens, 0);

        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let stats = server.stats();
        assert_eq!(stats.clients, 1);
    }

    #[test]
    fn test_code_challenge_method_parse() {
        assert_eq!(
            CodeChallengeMethod::parse("plain"),
            Some(CodeChallengeMethod::Plain)
        );
        assert_eq!(
            CodeChallengeMethod::parse("S256"),
            Some(CodeChallengeMethod::S256)
        );
        assert_eq!(CodeChallengeMethod::parse("unknown"), None);
    }

    #[test]
    fn test_oauth_error_display() {
        let err = OAuthError::InvalidRequest("missing parameter".to_string());
        assert_eq!(err.error_code(), "invalid_request");
        assert_eq!(err.description(), "missing parameter");
        assert_eq!(err.to_string(), "invalid_request: missing parameter");
    }

    #[test]
    fn test_token_revocation() {
        let server = Arc::new(OAuthServer::with_defaults());

        // Register a client
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_response = issue_access_token_via_auth_code(
            server.as_ref(),
            "test-client",
            "http://127.0.0.1:3000/callback",
            &["read"],
            "user123",
        );

        // Token should be valid
        assert!(
            server
                .validate_access_token(&token_response.access_token)
                .is_some()
        );

        // Revoke the token
        server
            .revoke(&token_response.access_token, "test-client", None)
            .unwrap();

        // Token should no longer be valid
        assert!(
            server
                .validate_access_token(&token_response.access_token)
                .is_none()
        );
    }

    #[test]
    fn test_client_unregistration() {
        let server = OAuthServer::with_defaults();

        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        assert!(server.get_client("test-client").is_some());

        server.unregister_client("test-client").unwrap();

        assert!(server.get_client("test-client").is_none());

        // Unregistering again should fail
        assert!(server.unregister_client("test-client").is_err());
    }

    #[test]
    fn test_token_verifier() {
        let server = Arc::new(OAuthServer::with_defaults());

        // Register a client and create a token
        let client = OAuthClient::builder("test-client")
            .redirect_uri("http://127.0.0.1:3000/callback")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_response = issue_access_token_via_auth_code(
            server.as_ref(),
            "test-client",
            "http://127.0.0.1:3000/callback",
            &["read"],
            "user123",
        );

        // Create verifier
        let verifier = server.token_verifier(configured_resource(TEST_MCP_RESOURCE));
        let cx = asupersync::Cx::for_testing();
        let mcp_ctx = McpContext::new(cx, 1);
        let auth_request = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };

        // Valid token
        let access = AccessToken {
            scheme: "Bearer".to_string(),
            token: token_response.access_token.clone(),
        };
        let result = verifier.verify(&mcp_ctx, auth_request, &access);
        assert!(result.is_ok());
        let auth = result.unwrap();
        assert_eq!(auth.subject, Some("oauth-test-subject".to_string()));
        assert_eq!(auth.scopes, vec!["read".to_string()]);

        // Invalid token
        let invalid = AccessToken {
            scheme: "Bearer".to_string(),
            token: "invalid-value".to_string(),
        };
        let result = verifier.verify(&mcp_ctx, auth_request, &invalid);
        assert!(result.is_err());

        // Wrong scheme
        let wrong_scheme = AccessToken {
            scheme: "Basic".to_string(),
            token: token_response.access_token,
        };
        let result = verifier.verify(&mcp_ctx, auth_request, &wrong_scheme);
        assert!(result.is_err());
    }

    // ========================================
    // OAuthServerConfig
    // ========================================

    #[test]
    fn config_default_values() {
        let c = OAuthServerConfig::default();
        assert_eq!(c.issuer, "https://fastmcp.invalid/");
        assert_eq!(c.access_token_lifetime, Duration::from_mins(15));
        assert_eq!(c.refresh_token_lifetime, Duration::from_hours(720));
        assert_eq!(c.authorization_code_lifetime, Duration::from_mins(5));
        assert!(c.allow_public_clients);
        assert_eq!(c.min_code_verifier_length, 43);
        assert_eq!(c.max_code_verifier_length, 128);
        assert_eq!(c.max_clients, DEFAULT_MAX_OAUTH_CLIENTS);
        assert_eq!(c.max_authorization_codes, DEFAULT_MAX_AUTHORIZATION_CODES);
        assert_eq!(
            c.max_authorization_codes_per_client,
            DEFAULT_MAX_AUTHORIZATION_CODES_PER_CLIENT
        );
        assert_eq!(c.max_access_tokens, DEFAULT_MAX_ACCESS_TOKENS);
        assert_eq!(
            c.max_access_tokens_per_client,
            DEFAULT_MAX_ACCESS_TOKENS_PER_CLIENT
        );
        assert_eq!(c.max_refresh_tokens, DEFAULT_MAX_REFRESH_TOKENS);
        assert_eq!(
            c.max_refresh_tokens_per_client,
            DEFAULT_MAX_REFRESH_TOKENS_PER_CLIENT
        );
        assert_eq!(
            c.max_revocation_tombstones,
            DEFAULT_MAX_REVOCATION_TOMBSTONES
        );
        assert_eq!(
            c.max_revocation_tombstones_per_client,
            DEFAULT_MAX_REVOCATION_TOMBSTONES_PER_CLIENT
        );
        assert!(c.validate().is_ok());
    }

    #[test]
    fn config_debug_and_clone() {
        let c = OAuthServerConfig::default();
        let debug = format!("{:?}", c);
        assert!(debug.contains("OAuthServerConfig"));
        assert!(debug.contains("https://fastmcp.invalid"));

        let cloned = c.clone();
        assert_eq!(cloned.issuer, "https://fastmcp.invalid/");
    }

    #[test]
    fn config_enforces_rfc7636_verifier_bounds_and_ordering() {
        let invalid = [
            OAuthServerConfig {
                min_code_verifier_length: PKCE_CODE_VERIFIER_MIN_BYTES - 1,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                max_code_verifier_length: PKCE_CODE_VERIFIER_MAX_BYTES + 1,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                min_code_verifier_length: PKCE_CODE_VERIFIER_MAX_BYTES,
                max_code_verifier_length: PKCE_CODE_VERIFIER_MIN_BYTES,
                ..OAuthServerConfig::default()
            },
        ];
        for config in invalid {
            let error = config.validate().expect_err("invalid PKCE policy");
            assert!(matches!(&error, OAuthError::ServerError(_)));
            assert!(error.description().contains("PKCE verifier bounds"));
        }

        OAuthServerConfig {
            min_code_verifier_length: PKCE_CODE_VERIFIER_MIN_BYTES,
            max_code_verifier_length: PKCE_CODE_VERIFIER_MIN_BYTES,
            ..OAuthServerConfig::default()
        }
        .validate()
        .expect("a coherent strict RFC 7636 subset is valid");
    }

    #[test]
    fn config_enforces_nonzero_bounded_coherent_lifetimes() {
        let invalid = [
            OAuthServerConfig {
                access_token_lifetime: Duration::ZERO,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                refresh_token_lifetime: Duration::ZERO,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                authorization_code_lifetime: Duration::ZERO,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                access_token_lifetime: Duration::from_millis(999),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                access_token_lifetime: Duration::from_secs(1),
                refresh_token_lifetime: Duration::from_millis(999),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                authorization_code_lifetime: Duration::from_millis(999),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                access_token_lifetime: MAX_ACCESS_TOKEN_LIFETIME + Duration::from_secs(1),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                refresh_token_lifetime: MAX_REFRESH_TOKEN_LIFETIME + Duration::from_secs(1),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                authorization_code_lifetime: MAX_AUTHORIZATION_CODE_LIFETIME
                    + Duration::from_secs(1),
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                access_token_lifetime: Duration::from_secs(2),
                refresh_token_lifetime: Duration::from_secs(1),
                ..OAuthServerConfig::default()
            },
        ];
        for config in invalid {
            assert!(matches!(config.validate(), Err(OAuthError::ServerError(_))));
        }

        OAuthServerConfig {
            access_token_lifetime: MAX_ACCESS_TOKEN_LIFETIME,
            refresh_token_lifetime: MAX_REFRESH_TOKEN_LIFETIME,
            authorization_code_lifetime: MAX_AUTHORIZATION_CODE_LIFETIME,
            ..OAuthServerConfig::default()
        }
        .validate()
        .expect("exact lifetime ceilings are admitted");

        OAuthServerConfig {
            access_token_lifetime: Duration::from_secs(1),
            refresh_token_lifetime: Duration::from_secs(1),
            authorization_code_lifetime: Duration::from_secs(1),
            ..OAuthServerConfig::default()
        }
        .validate()
        .expect("one-second credential lifetimes are admitted");
    }

    #[test]
    fn one_second_token_lifetime_has_positive_wire_expiry() {
        let server = OAuthServer::new(OAuthServerConfig {
            access_token_lifetime: Duration::from_secs(1),
            refresh_token_lifetime: Duration::from_secs(1),
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();

        let response = server.issue_tokens("c1", &[], None).unwrap();

        assert_eq!(response.expires_in, 1);
    }

    #[test]
    fn config_rejects_per_client_caps_above_global_caps() {
        let invalid = [
            OAuthServerConfig {
                max_authorization_codes: 1,
                max_authorization_codes_per_client: 2,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                max_access_tokens: 1,
                max_access_tokens_per_client: 2,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                max_refresh_tokens: 1,
                max_refresh_tokens_per_client: 2,
                ..OAuthServerConfig::default()
            },
            OAuthServerConfig {
                max_revocation_tombstones: 1,
                max_revocation_tombstones_per_client: 2,
                ..OAuthServerConfig::default()
            },
        ];
        for config in invalid {
            let error = config.validate().expect_err("incoherent state cap");
            assert!(matches!(&error, OAuthError::ServerError(_)));
            assert!(error.description().contains("must not exceed"));
        }
    }

    // ========================================
    // ClientType
    // ========================================

    #[test]
    fn client_type_debug_and_eq() {
        assert_eq!(ClientType::Public, ClientType::Public);
        assert_ne!(ClientType::Public, ClientType::Confidential);
        let debug = format!("{:?}", ClientType::Confidential);
        assert!(debug.contains("Confidential"));
    }

    #[test]
    fn client_type_copy() {
        let t = ClientType::Public;
        let t2 = t; // Copy
        assert_eq!(t, t2);
    }

    // ========================================
    // OAuthClient — additional
    // ========================================

    #[test]
    fn client_debug_is_redacted_and_client_is_not_consumed() {
        let client = OAuthClient::builder("dbg")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        let debug = format!("{:?}", client);
        assert!(debug.contains("OAuthClient"));
        assert!(debug.contains("client_id_len"));
        assert!(!debug.contains("dbg"));

        assert_eq!(client.client_id, "dbg");
    }

    #[test]
    fn client_authenticate_public_no_secret() {
        let client = OAuthClient::builder("pub")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        // Public client with no secret provided: should succeed
        assert!(client.authenticate(None));
        // Public client with secret provided: should fail
        assert!(!client.authenticate(Some("any")));
    }

    #[test]
    fn client_validate_redirect_uri_non_localhost() {
        let client = OAuthClient::builder("c")
            .redirect_uri("https://example.com/cb")
            .build()
            .unwrap();
        // Non-loopback redirects require an exact match.
        assert!(client.validate_redirect_uri("https://example.com/cb"));
        assert!(!client.validate_redirect_uri("https://example.com/cb2"));
        assert!(!client.validate_redirect_uri("https://other.com/cb"));
    }

    #[test]
    fn client_validate_redirect_uri_localhost_ipv6() {
        let client = OAuthClient::builder("c")
            .redirect_uri("http://[::1]:3000/callback")
            .build()
            .unwrap();
        // IPv6 loopback with a different port.
        assert!(client.validate_redirect_uri("http://[::1]:8080/callback"));
        // A hostname or different IP family may not borrow the port exception.
        assert!(!client.validate_redirect_uri("http://localhost:9000/callback"));
        assert!(!client.validate_redirect_uri("http://127.0.0.1:9000/callback"));
    }

    #[test]
    fn client_validate_scopes_empty() {
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        // Empty scopes should always be valid
        assert!(client.validate_scopes(&[]));
    }

    // ========================================
    // OAuthClientBuilder — additional
    // ========================================

    #[test]
    fn client_builder_debug() {
        let builder = OAuthClient::builder("test-id");
        let debug = format!("{:?}", builder);
        assert!(debug.contains("OAuthClientBuilder"));
        assert!(debug.contains("client_id_len"));
        assert!(!debug.contains("test-id"));
    }

    #[test]
    fn client_builder_empty_id_fails() {
        let result = OAuthClient::builder("")
            .redirect_uri("http://127.0.0.1/cb")
            .build();
        assert!(result.is_err());
    }

    #[test]
    fn client_builder_no_redirect_uris_fails() {
        let result = OAuthClient::builder("c").build();
        assert!(result.is_err());
    }

    #[test]
    fn client_builder_redirect_uris_multiple() {
        let client = OAuthClient::builder("c")
            .redirect_uris(vec!["http://127.0.0.1/a", "http://127.0.0.1/b"])
            .build()
            .unwrap();
        assert_eq!(client.redirect_uris.len(), 2);
    }

    #[test]
    fn client_builder_scopes_multiple() {
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .scopes(vec!["r", "w", "admin"])
            .build()
            .unwrap();
        assert_eq!(client.allowed_scopes.len(), 3);
    }

    #[test]
    fn client_builder_description() {
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .description("A test app")
            .build()
            .unwrap();
        assert_eq!(client.description, Some("A test app".to_string()));
    }

    // ========================================
    // CodeChallengeMethod — additional
    // ========================================

    #[test]
    fn code_challenge_method_as_str() {
        assert_eq!(CodeChallengeMethod::Plain.as_str(), "plain");
        assert_eq!(CodeChallengeMethod::S256.as_str(), "S256");
    }

    #[test]
    fn code_challenge_method_clone_copy_eq() {
        let m = CodeChallengeMethod::S256;
        let m2 = m; // Copy
        assert_eq!(m, m2);
        let m3 = m.clone();
        assert_eq!(m, m3);
    }

    // ========================================
    // AuthorizationCode
    // ========================================

    #[test]
    fn authorization_code_not_expired_initially() {
        let code = AuthorizationCode {
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            code_challenge: "challenge".to_string(),
            code_challenge_method: CodeChallengeMethod::Plain,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(600))
                .expect("test deadline"),
            subject: None,
            state: None,
            registration_epoch: test_registration_epoch(1),
        };
        assert!(!code.is_expired());
    }

    #[test]
    fn authorization_code_expired() {
        let code = AuthorizationCode {
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            code_challenge: "challenge".to_string(),
            code_challenge_method: CodeChallengeMethod::Plain,
            issued_at: Instant::now() - Duration::from_secs(100),
            expires_at: Instant::now() - Duration::from_secs(1),
            subject: None,
            state: None,
            registration_epoch: test_registration_epoch(1),
        };
        assert!(code.is_expired());
    }

    #[test]
    fn authorization_code_rejects_plain_verifier_method() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let code = AuthorizationCode {
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            code_challenge: verifier.to_string(),
            code_challenge_method: CodeChallengeMethod::Plain,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(600))
                .expect("test deadline"),
            subject: None,
            state: None,
            registration_epoch: test_registration_epoch(1),
        };
        assert!(!code.validate_code_verifier(verifier));
        assert!(!code.validate_code_verifier("wrong"));
    }

    #[test]
    fn authorization_code_validate_s256() {
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_s256_challenge(verifier).unwrap();
        let code = AuthorizationCode {
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            code_challenge: challenge,
            code_challenge_method: CodeChallengeMethod::S256,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(600))
                .expect("test deadline"),
            subject: None,
            state: None,
            registration_epoch: test_registration_epoch(1),
        };
        assert!(code.validate_code_verifier(verifier));
        assert!(!code.validate_code_verifier("wrong-verifier"));
    }

    #[test]
    fn authorization_code_debug_and_clone() {
        let code = AuthorizationCode {
            client_id: "cid".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec!["read".to_string()],
            resource: None,
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::Plain,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(60))
                .expect("test deadline"),
            subject: Some("user".to_string()),
            state: Some("state".to_string()),
            registration_epoch: test_registration_epoch(1),
        };
        let debug = format!("{:?}", code);
        assert!(debug.contains("AuthorizationCode"));
        let cloned = code.clone();
        assert_eq!(cloned.client_id, "cid");
    }

    // ========================================
    // TokenType
    // ========================================

    #[test]
    fn token_type_as_str() {
        assert_eq!(TokenType::Bearer.as_str(), "bearer");
    }

    #[test]
    fn token_type_debug_clone_copy_eq() {
        let t = TokenType::Bearer;
        let t2 = t; // Copy
        assert_eq!(t, t2);
        let t3 = t.clone();
        assert_eq!(t, t3);
        let debug = format!("{:?}", t);
        assert!(debug.contains("Bearer"));
    }

    // ========================================
    // OAuthToken
    // ========================================

    #[test]
    fn oauth_token_not_expired() {
        let token = OAuthToken {
            token: String::new(),
            token_type: TokenType::Bearer,
            client_id: "c".to_string(),
            scopes: vec![],
            resource: None,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(3600))
                .expect("test deadline"),
            subject: None,
            is_refresh_token: false,
        };
        assert!(!token.is_expired());
        assert!(token.expires_in_secs() > 0);
    }

    #[test]
    fn oauth_token_expired() {
        let token = OAuthToken {
            token: String::new(),
            token_type: TokenType::Bearer,
            client_id: "c".to_string(),
            scopes: vec![],
            resource: None,
            issued_at: Instant::now() - Duration::from_secs(100),
            expires_at: Instant::now() - Duration::from_secs(1),
            subject: None,
            is_refresh_token: false,
        };
        assert!(token.is_expired());
        assert_eq!(token.expires_in_secs(), 0);
    }

    #[test]
    fn oauth_token_debug_and_clone() {
        let token = OAuthToken {
            token: String::new(),
            token_type: TokenType::Bearer,
            client_id: "c".to_string(),
            scopes: vec!["read".to_string()],
            resource: None,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(60))
                .expect("test deadline"),
            subject: Some("user".to_string()),
            is_refresh_token: true,
        };
        let debug = format!("{:?}", token);
        assert!(debug.contains("OAuthToken"));
        let cloned = token.clone();
        assert_eq!(cloned.client_id, "c");
        assert!(cloned.is_refresh_token);
    }

    // ========================================
    // TokenResponse
    // ========================================

    #[test]
    fn token_response_serialize_without_optional_fields() {
        let resp = TokenResponse {
            access_token: "at".to_string(),
            token_type: "bearer".to_string(),
            expires_in: 3600,
            refresh_token: None,
            scope: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(!json.contains("refresh_token"));
        assert!(!json.contains("scope"));
    }

    #[test]
    fn token_response_serialize_with_optional_fields() {
        let resp = TokenResponse {
            access_token: "at".to_string(),
            token_type: "bearer".to_string(),
            expires_in: 3600,
            refresh_token: Some("rt".to_string()),
            scope: Some("read write".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("refresh_token"));
        assert!(json.contains("scope"));
    }

    // ========================================
    // AuthorizationRequest / TokenRequest
    // ========================================

    #[test]
    fn authorization_request_debug_and_clone() {
        let req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec!["read".to_string()],
            resource: None,
            state: Some("s".to_string()),
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let debug = format!("{:?}", req);
        assert!(debug.contains("AuthorizationRequest"));
        let cloned = req.clone();
        assert_eq!(cloned.client_id, "c");
    }

    #[test]
    fn token_request_debug_is_redacted() {
        let req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some("code".to_string()),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some("verifier".to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let debug = format!("{:?}", req);
        assert!(debug.contains("TokenRequest"));
        assert_eq!(req.grant_type, "authorization_code");
    }

    #[test]
    fn oauth_debug_surfaces_redact_secret_and_identity_canaries() {
        const CANARY: &str = "oauth-debug-secret-identity-canary";
        let now = Instant::now();
        let client = OAuthClient::builder(format!("client-{CANARY}"))
            .secret(format!("secret-{CANARY}"))
            .redirect_uri(format!("https://{CANARY}.example/callback"))
            .scope(format!("scope-{CANARY}"))
            .name(format!("name-{CANARY}"))
            .description(format!("description-{CANARY}"))
            .build()
            .unwrap();
        let client_metadata = OAuthClientMetadata::from(&client);
        let builder = OAuthClient::builder(format!("builder-{CANARY}"))
            .secret(format!("builder-secret-{CANARY}"))
            .redirect_uri(format!("https://{CANARY}.example/builder"))
            .scope(format!("builder-scope-{CANARY}"))
            .name(format!("builder-name-{CANARY}"))
            .description(format!("builder-description-{CANARY}"));
        let authorization_code = AuthorizationCode {
            client_id: format!("client-{CANARY}"),
            redirect_uri: format!("https://{CANARY}.example/code"),
            scopes: vec![format!("scope-{CANARY}")],
            resource: None,
            code_challenge: format!("challenge-{CANARY}"),
            code_challenge_method: CodeChallengeMethod::S256,
            issued_at: now,
            expires_at: now
                .checked_add(Duration::from_secs(60))
                .expect("test deadline"),
            subject: Some(format!("subject-{CANARY}")),
            state: Some(format!("state-{CANARY}")),
            registration_epoch: test_registration_epoch(1),
        };
        let oauth_token = OAuthToken {
            token: String::new(),
            token_type: TokenType::Bearer,
            client_id: format!("client-{CANARY}"),
            scopes: vec![format!("scope-{CANARY}")],
            resource: None,
            issued_at: now,
            expires_at: now
                .checked_add(Duration::from_secs(60))
                .expect("test deadline"),
            subject: Some(format!("subject-{CANARY}")),
            is_refresh_token: true,
        };
        let token_response = TokenResponse {
            access_token: format!("access-{CANARY}"),
            token_type: format!("type-{CANARY}"),
            expires_in: 60,
            refresh_token: Some(format!("refresh-{CANARY}")),
            scope: Some(format!("scope-{CANARY}")),
        };
        let authorization_request = AuthorizationRequest {
            response_type: format!("response-{CANARY}"),
            client_id: format!("client-{CANARY}"),
            redirect_uri: format!("https://{CANARY}.example/request"),
            scopes: vec![format!("scope-{CANARY}")],
            resource: None,
            state: Some(format!("state-{CANARY}")),
            code_challenge: format!("challenge-{CANARY}"),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let token_request = TokenRequest {
            grant_type: format!("grant-{CANARY}"),
            code: Some(format!("code-{CANARY}")),
            redirect_uri: Some(format!("https://{CANARY}.example/token")),
            client_id: format!("client-{CANARY}"),
            client_secret: Some(format!("secret-{CANARY}")),
            code_verifier: Some(format!("verifier-{CANARY}")),
            refresh_token: Some(format!("refresh-{CANARY}")),
            scopes: Some(vec![format!("scope-{CANARY}")]),
            resource: None,
        };
        let error = OAuthError::InvalidGrant(format!("error-{CANARY}"));

        let wire = serde_json::to_value(&token_response).unwrap();
        assert_eq!(wire["access_token"], format!("access-{CANARY}"));
        assert_eq!(wire["refresh_token"], format!("refresh-{CANARY}"));

        let debug_outputs = [
            format!("{client:?}"),
            format!("{client_metadata:?}"),
            format!("{builder:?}"),
            format!("{authorization_code:?}"),
            format!("{oauth_token:?}"),
            format!("{token_response:?}"),
            format!("{authorization_request:?}"),
            format!("{token_request:?}"),
            format!("{error:?}"),
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
    }

    // ========================================
    // OAuthError — additional
    // ========================================

    #[test]
    fn oauth_error_all_codes() {
        let cases: Vec<(OAuthError, &str)> = vec![
            (OAuthError::InvalidRequest("x".into()), "invalid_request"),
            (OAuthError::InvalidClient("x".into()), "invalid_client"),
            (OAuthError::InvalidGrant("x".into()), "invalid_grant"),
            (
                OAuthError::UnauthorizedClient("x".into()),
                "unauthorized_client",
            ),
            (
                OAuthError::UnsupportedGrantType("x".into()),
                "unsupported_grant_type",
            ),
            (OAuthError::InvalidScope("x".into()), "invalid_scope"),
            (OAuthError::ServerError("x".into()), "server_error"),
            (
                OAuthError::TemporarilyUnavailable("x".into()),
                "temporarily_unavailable",
            ),
            (OAuthError::AccessDenied("x".into()), "access_denied"),
            (
                OAuthError::UnsupportedResponseType("x".into()),
                "unsupported_response_type",
            ),
        ];
        for (err, expected_code) in cases {
            assert_eq!(err.error_code(), expected_code);
            assert_eq!(err.description(), "x");
        }
    }

    #[test]
    fn oauth_error_debug_and_clone() {
        let err = OAuthError::ServerError("test".into());
        let debug = format!("{:?}", err);
        assert!(debug.contains("ServerError"));
        let cloned = err.clone();
        assert_eq!(cloned.description(), "test");
    }

    #[test]
    fn oauth_error_is_std_error() {
        let err = OAuthError::InvalidGrant("x".into());
        let _: &dyn std::error::Error = &err;
    }

    #[test]
    fn oauth_error_into_mcp_error_forbidden() {
        // InvalidClient and UnauthorizedClient and AccessDenied → ResourceForbidden
        let err: McpError = OAuthError::InvalidClient("c".into()).into();
        assert!(err.message.contains("invalid_client"));
        let err: McpError = OAuthError::UnauthorizedClient("c".into()).into();
        assert!(err.message.contains("unauthorized_client"));
        let err: McpError = OAuthError::AccessDenied("d".into()).into();
        assert!(err.message.contains("access_denied"));
    }

    #[test]
    fn oauth_error_into_mcp_error_invalid_request() {
        // Other variants → InvalidRequest
        let err: McpError = OAuthError::InvalidScope("s".into()).into();
        assert!(err.message.contains("invalid_scope"));
        let err: McpError = OAuthError::UnsupportedGrantType("g".into()).into();
        assert!(err.message.contains("unsupported_grant_type"));
    }

    // ========================================
    // OAuthServer — additional
    // ========================================

    #[test]
    fn server_config_accessor() {
        let config = OAuthServerConfig {
            issuer: "https://issuer.example/".to_string(),
            ..OAuthServerConfig::default()
        };
        let server = OAuthServer::new(config);
        assert_eq!(server.config().issuer, "https://issuer.example/");
    }

    #[test]
    fn server_register_public_not_allowed() {
        let config = OAuthServerConfig {
            allow_public_clients: false,
            ..OAuthServerConfig::default()
        };
        let server = OAuthServer::new(config);

        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        let result = server.register_client(client);
        assert!(matches!(result, Err(OAuthError::InvalidClient(_))));
    }

    #[test]
    fn server_list_clients() {
        let server = OAuthServer::with_defaults();
        assert_eq!(server.list_clients().len(), 0);

        let client = OAuthClient::builder("a")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        assert_eq!(server.list_clients().len(), 1);
    }

    #[test]
    fn server_authorize_unsupported_response_type() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let req = AuthorizationRequest {
            response_type: "token".to_string(), // not "code"
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let result = server.authorize(&req);
        assert!(matches!(
            result,
            Err(OAuthError::UnsupportedResponseType(_))
        ));
    }

    #[test]
    fn server_authorize_invalid_redirect() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "https://evil.com/cb".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let result = server.authorize(&req);
        assert!(matches!(result, Err(OAuthError::InvalidRequest(_))));
    }

    #[test]
    fn server_authorize_invalid_scope() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec!["admin".to_string()],
            resource: None,
            state: None,
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let result = server.authorize(&req);
        assert!(matches!(result, Err(OAuthError::InvalidScope(_))));
    }

    #[test]
    fn server_authorize_unknown_client() {
        let server = OAuthServer::with_defaults();
        let req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "nonexistent".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: "ch".to_string(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let result = server.authorize(&req);
        assert!(matches!(result, Err(OAuthError::InvalidClient(_))));
    }

    #[test]
    fn server_token_unsupported_grant_type() {
        let server = OAuthServer::with_defaults();
        let req = TokenRequest {
            grant_type: "client_credentials".to_string(),
            code: None,
            redirect_uri: None,
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: None,
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let result = server.token(&req);
        assert!(matches!(result, Err(OAuthError::UnsupportedGrantType(_))));
    }

    #[test]
    fn server_token_auth_code_missing_code() {
        let server = OAuthServer::with_defaults();
        let req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: None, // missing
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some("v".repeat(43)),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let result = server.token(&req);
        assert!(matches!(result, Err(OAuthError::InvalidRequest(_))));
    }

    #[test]
    fn server_token_auth_code_missing_redirect() {
        let server = OAuthServer::with_defaults();
        let req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some("code".to_string()),
            redirect_uri: None, // missing
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some("v".repeat(43)),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let result = server.token(&req);
        assert!(matches!(result, Err(OAuthError::InvalidRequest(_))));
    }

    #[test]
    fn server_token_auth_code_missing_verifier() {
        let server = OAuthServer::with_defaults();
        let req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some("code".to_string()),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: None, // missing
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let result = server.token(&req);
        assert!(matches!(result, Err(OAuthError::InvalidRequest(_))));
    }

    #[test]
    fn server_token_auth_code_verifier_too_short() {
        let mut config = OAuthServerConfig::default();
        // Fail-closed validation now rejects configs below the RFC 7636
        // 43-byte floor outright, so the most permissive LEGAL configuration
        // is the floor itself; the short verifier below must still bounce.
        config.min_code_verifier_length = PKCE_CODE_VERIFIER_MIN_BYTES;
        let server = OAuthServer::with_approval_backend(
            config,
            Arc::new(TestDefaultAuthorizationApprovalBackend),
        );
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Authorize first
        let issued_verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let verifier = "short"; // The fixed 43-byte minimum still applies.
        let req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: compute_s256_challenge(issued_verifier).unwrap(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let (code, _) = server.authorize(&req).unwrap();
        let stored_code = code.clone();

        let token_req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some(code),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some(verifier.to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let result = server.token(&token_req);
        // PKCE verifier failures on the token endpoint uniformly surface as
        // invalid_grant (RFC 7636 section 4.6), keeping length and mismatch
        // rejections indistinguishable to a probing client.
        assert!(matches!(result, Err(OAuthError::InvalidGrant(_))));
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&stored_code))
        );
    }

    #[test]
    fn server_full_auth_code_flow_with_s256() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let challenge = compute_s256_challenge(verifier).unwrap();

        let auth_req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec!["read".to_string()],
            resource: None,
            state: None,
            code_challenge: challenge,
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let (code, _) = server.authorize(&auth_req).unwrap();

        let token_req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some(code),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some(verifier.to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        let resp = server.token(&token_req).unwrap();
        assert_ne!(resp.access_token.len(), 0);
        assert!(resp.refresh_token.is_some());
        assert_eq!(resp.token_type, "bearer");
        assert_eq!(resp.scope, Some("read".to_string()));
    }

    #[test]
    fn server_token_code_already_used() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let auth_req = AuthorizationRequest {
            response_type: "code".to_string(),
            client_id: "c".to_string(),
            redirect_uri: "http://127.0.0.1/cb".to_string(),
            scopes: vec![],
            resource: None,
            state: None,
            code_challenge: compute_s256_challenge(verifier).unwrap(),
            code_challenge_method: CodeChallengeMethod::S256,
        };
        let (code, _) = server.authorize(&auth_req).unwrap();

        let token_req = TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some(code.clone()),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c".to_string(),
            client_secret: None,
            code_verifier: Some(verifier.to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        };
        // First use succeeds
        server.token(&token_req).unwrap();
        // Second use fails (code is single-use)
        let result = server.token(&token_req);
        assert!(matches!(result, Err(OAuthError::InvalidGrant(_))));
    }

    #[test]
    fn server_validate_access_token_nonexistent() {
        let server = OAuthServer::with_defaults();
        assert!(server.validate_access_token("nonexistent").is_none());
    }

    #[test]
    fn server_unregister_client_revokes_tokens() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let resp = issue_access_token_via_auth_code(
            &server,
            "c",
            "http://127.0.0.1/cb",
            &["read"],
            "user",
        );
        assert!(server.validate_access_token(&resp.access_token).is_some());

        server.unregister_client("c").unwrap();
        assert!(server.validate_access_token(&resp.access_token).is_none());
    }

    #[test]
    fn server_cleanup_expired_removes_old_tokens() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let response =
            issue_access_token_via_auth_code(&server, "c", "http://127.0.0.1/cb", &[], "user");
        let refresh = response.refresh_token.expect("refresh token");
        let expired_at = Instant::now();
        {
            let mut state = server.state.write().unwrap();
            let access = state
                .access_tokens
                .get_mut(&access_token_digest(&response.access_token))
                .expect("stored access token");
            access.metadata.expires_at = expired_at;
            access.family_expires_at = expired_at;
            let refresh = state
                .refresh_tokens
                .get_mut(&refresh_token_digest(&refresh))
                .expect("stored refresh token");
            refresh.metadata.expires_at = expired_at;
            refresh.family_expires_at = expired_at;
        }

        let stats_before = server.stats();
        server.cleanup_expired();
        let stats_after = server.stats();

        assert_eq!(stats_before.access_tokens, 1);
        assert_eq!(stats_before.refresh_tokens, 1);
        assert_eq!(stats_after.access_tokens, 0);
        assert_eq!(stats_after.refresh_tokens, 0);
    }

    // ========================================
    // OAuthServerStats
    // ========================================

    #[test]
    fn server_stats_default() {
        let stats = OAuthServerStats::default();
        assert_eq!(stats.clients, 0);
        assert_eq!(stats.authorization_codes, 0);
        assert_eq!(stats.access_tokens, 0);
        assert_eq!(stats.refresh_tokens, 0);
        assert_eq!(stats.revoked_tokens, 0);
    }

    #[test]
    fn server_stats_debug_and_clone() {
        let stats = OAuthServerStats {
            clients: 1,
            access_tokens: 5,
            ..OAuthServerStats::default()
        };
        let debug = format!("{:?}", stats);
        assert!(debug.contains("OAuthServerStats"));
        let cloned = stats.clone();
        assert_eq!(cloned.clients, 1);
    }

    // ========================================
    // Helper functions — additional
    // ========================================

    #[test]
    fn is_loopback_redirect_tests() {
        assert!(!is_loopback_redirect("http://localhost:3000/cb"));
        assert!(is_loopback_redirect("http://127.0.0.1:8080/cb"));
        assert!(is_loopback_redirect("http://[::1]:9000/cb"));
        assert!(!is_loopback_redirect("https://example.com/cb"));
        assert!(!is_loopback_redirect("http://evil.com/cb"));
        assert!(!is_loopback_redirect(
            "http://localhost:3000@evil.example/cb"
        ));
        assert!(!is_loopback_redirect("http://localhost.evil.example/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1:not-a-port/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1:+80/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1:65536/cb"));
        assert!(!is_loopback_redirect("http://127.0.0.1:3000/cb#fragment"));
    }

    #[test]
    fn compute_s256_challenge_deterministic() {
        let v = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let c1 = compute_s256_challenge(v).unwrap();
        let c2 = compute_s256_challenge(v).unwrap();
        assert_eq!(c1, c2);
        assert_ne!(c1.len(), 0);
    }

    #[test]
    fn url_encode_special_chars() {
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a+b"), "a%2Bb");
        assert_eq!(url_encode("a/b"), "a%2Fb");
        assert_eq!(url_encode("safe-_~."), "safe-_~.");
    }

    #[test]
    fn constant_time_eq_same_length_different() {
        assert!(!constant_time_eq("abc", "abd"));
    }

    #[test]
    fn loopback_match_different_paths_fail() {
        assert!(!loopback_match(
            "http://127.0.0.1:3000/a",
            "http://127.0.0.1:3000/b"
        ));
        assert!(!loopback_match(
            "http://127.0.0.1:3000/cb",
            "http://localhost:3000@evil.example/cb"
        ));
    }

    #[test]
    fn loopback_match_non_http_fails() {
        assert!(!loopback_match("ftp://127.0.0.1/a", "ftp://127.0.0.1/a"));
    }

    // ========================================
    // Refresh token flow
    // ========================================

    #[test]
    fn server_refresh_token_flow() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .scope("write")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read", "write"],
            "user1",
        );
        let refresh = token_resp.refresh_token.unwrap();

        // Use refresh token to get a new access token
        let new_resp = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh.clone()),
                scopes: None,
                resource: None,
            })
            .unwrap();

        // New access token should be different
        assert_ne!(new_resp.access_token, token_resp.access_token);
        assert_eq!(new_resp.token_type, "bearer");
        // Successful refresh rotates the credential.
        let rotated_refresh = new_resp
            .refresh_token
            .as_deref()
            .expect("refresh flow must rotate the refresh token");
        assert_ne!(rotated_refresh, refresh);
        // Scopes preserved
        assert!(new_resp.scope.is_some());
    }

    #[test]
    fn server_refresh_token_scope_narrowing() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .scope("write")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read", "write"],
            "user1",
        );
        let refresh = token_resp.refresh_token.unwrap();

        // Request only a subset of scopes
        let new_resp = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: Some(vec!["read".to_string()]),
                resource: None,
            })
            .unwrap();

        assert_eq!(new_resp.scope, Some("read".to_string()));
    }

    #[test]
    fn server_refresh_token_invalid_scope() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let refresh = token_resp.refresh_token.unwrap();

        // Request scope not in original grant
        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: Some(vec!["admin".to_string()]),
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_scope");
    }

    #[test]
    fn server_refresh_token_revoked() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let access = token_resp.access_token.clone();
        let refresh = token_resp.refresh_token.unwrap();

        // Revoking a refresh token invalidates the complete grant family.
        server.revoke(&refresh, "c1", None).unwrap();
        assert!(server.validate_access_token(&access).is_none());

        // Refresh should now fail
        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_grant");
        assert_eq!(err.description(), OAUTH_INVALID_GRANT_ERROR);
    }

    #[test]
    fn server_refresh_token_client_id_mismatch() {
        let server = OAuthServer::with_defaults();
        let client1 = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        let client2 = OAuthClient::builder("c2")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client1).unwrap();
        server.register_client(client2).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let refresh = token_resp.refresh_token.unwrap();

        // Try to use with different client_id
        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c2".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_grant");
        assert_eq!(err.description(), OAUTH_INVALID_GRANT_ERROR);
    }

    #[test]
    fn server_refresh_token_missing_param() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_request");
        assert!(err.description().contains("refresh_token"));
    }

    #[test]
    fn server_refresh_token_not_found() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some("nonexistent".to_string()),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_grant");
    }

    // ========================================
    // Token exchange edge cases
    // ========================================

    #[test]
    fn server_token_auth_code_redirect_uri_mismatch() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .redirect_uri("http://127.0.0.1/cb2")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Exchange with different redirect_uri
        let err = server
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code.clone()),
                redirect_uri: Some("http://127.0.0.1/cb2".to_string()),
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: Some(verifier.to_string()),
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_grant");
        assert_eq!(err.description(), OAUTH_INVALID_GRANT_ERROR);
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn server_token_auth_code_client_id_mismatch() {
        let server = OAuthServer::with_defaults();
        let client1 = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        let client2 = OAuthClient::builder("c2")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client1).unwrap();
        server.register_client(client2).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Exchange with different client_id
        let err = server
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code.clone()),
                redirect_uri: Some("http://127.0.0.1/cb".to_string()),
                client_id: "c2".to_string(),
                client_secret: None,
                code_verifier: Some(verifier.to_string()),
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_grant");
        assert_eq!(err.description(), OAUTH_INVALID_GRANT_ERROR);
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn server_token_auth_code_confidential_client_auth_fails() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .secret("correct-secret")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Exchange with wrong secret
        let err = server
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code.clone()),
                redirect_uri: Some("http://127.0.0.1/cb".to_string()),
                client_id: "c1".to_string(),
                client_secret: Some("wrong-secret".to_string()),
                code_verifier: Some(verifier.to_string()),
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_client");
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn failed_pkce_exchange_preserves_code_for_legitimate_single_use_retry() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec![],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        let request = |code_verifier: &str| TokenRequest {
            grant_type: "authorization_code".to_string(),
            code: Some(code.clone()),
            redirect_uri: Some("http://127.0.0.1/cb".to_string()),
            client_id: "c1".to_string(),
            client_secret: None,
            code_verifier: Some(code_verifier.to_string()),
            refresh_token: None,
            scopes: None,
            resource: None,
        };

        let malformed_verifier = "a".repeat(PKCE_CODE_VERIFIER_MIN_BYTES - 1);
        let error = server.token(&request(&malformed_verifier)).unwrap_err();
        assert!(matches!(&error, OAuthError::InvalidGrant(_)));
        assert_eq!(error.description(), OAUTH_INVALID_GRANT_ERROR);

        let wrong_verifier = "A".repeat(PKCE_CODE_VERIFIER_MIN_BYTES);
        let error = server.token(&request(&wrong_verifier)).unwrap_err();
        assert!(matches!(&error, OAuthError::InvalidGrant(_)));
        assert_eq!(error.description(), OAUTH_INVALID_GRANT_ERROR);
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );

        server.token(&request(verifier)).unwrap();
        assert!(
            !server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
        let replay = server.token(&request(verifier)).unwrap_err();
        assert!(matches!(&replay, OAuthError::InvalidGrant(_)));
        assert_eq!(replay.description(), OAUTH_INVALID_GRANT_ERROR);
    }

    #[test]
    fn config_cannot_weaken_fixed_rfc7636_verifier_maximum() {
        let mut config = OAuthServerConfig::default();
        config.max_code_verifier_length = usize::MAX;
        let error = config.validate().expect_err("RFC 7636 maximum is fixed");
        assert!(matches!(&error, OAuthError::ServerError(_)));
        assert!(error.description().contains("max_code_verifier_length"));
    }

    // ========================================
    // Authorization edge cases
    // ========================================

    #[test]
    fn server_authorize_empty_code_challenge() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let err = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: String::new(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_request");
        assert!(err.description().contains("code_challenge"));
    }

    #[test]
    fn server_authorize_with_state_in_redirect() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let (code, redirect) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: Some("my-csrf-state".to_string()),
                code_challenge: compute_s256_challenge(
                    "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                )
                .unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Redirect should contain code, state, and the RFC 9207 issuer binding.
        assert!(redirect.contains("code="));
        assert!(redirect.contains(&url_encode(&code)));
        assert!(redirect.contains("state=my-csrf-state"));
        let redirect = Url::parse(&redirect).unwrap();
        let issuers: Vec<_> = redirect
            .query_pairs()
            .filter(|(name, _)| name.as_ref() == "iss")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(issuers, [server.config().issuer.clone()]);
    }

    #[test]
    fn server_authorize_redirect_with_existing_query() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb?foo=bar")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let (_code, redirect) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb?foo=bar".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(
                    "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                )
                .unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Should use '&' separator since '?' already exists
        assert!(redirect.starts_with("http://127.0.0.1/cb?foo=bar&code="));
        assert!(redirect.contains("&iss=https%3A%2F%2Ffastmcp.invalid%2F"));
    }

    // ========================================
    // Error conversions
    // ========================================

    #[test]
    fn oauth_error_access_denied_into_mcp_error() {
        let err = OAuthError::AccessDenied("denied".to_string());
        let mcp: McpError = err.into();
        assert_eq!(mcp.code, McpErrorCode::ResourceForbidden);
    }

    #[test]
    fn oauth_error_description_all_variants() {
        let cases: Vec<(OAuthError, &str)> = vec![
            (OAuthError::ServerError("srv".into()), "srv"),
            (OAuthError::TemporarilyUnavailable("tmp".into()), "tmp"),
            (OAuthError::UnsupportedResponseType("rt".into()), "rt"),
        ];
        for (err, expected) in cases {
            assert_eq!(err.description(), expected);
        }
    }

    #[test]
    fn oauth_error_display_all_remaining_variants() {
        let err = OAuthError::TemporarilyUnavailable("try later".into());
        assert_eq!(format!("{err}"), "temporarily_unavailable: try later");

        let err = OAuthError::UnsupportedResponseType("bad".into());
        assert_eq!(format!("{err}"), "unsupported_response_type: bad");

        let err = OAuthError::AccessDenied("nope".into());
        assert_eq!(format!("{err}"), "access_denied: nope");
    }

    // ========================================
    // Revocation edge cases
    // ========================================

    #[test]
    fn server_revoke_unknown_token_succeeds() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Per RFC 7009, revoking an unknown token is not an error
        server.revoke("no-such-token", "c1", None).unwrap();
    }

    #[test]
    fn server_revoke_token_owned_by_other_client() {
        let server = OAuthServer::with_defaults();
        let client1 = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        let client2 = OAuthClient::builder("c2")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client1).unwrap();
        server.register_client(client2).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let refresh_token = token_resp
            .refresh_token
            .clone()
            .expect("authorization-code flow issues a refresh token");

        // c2 tries to revoke c1's tokens — both calls succeed silently, but
        // neither token may be removed or marked revoked.
        server.revoke(&token_resp.access_token, "c2", None).unwrap();
        server.revoke(&refresh_token, "c2", None).unwrap();

        // Token remains active and was not added to the global revocation set.
        assert!(
            server
                .validate_access_token(&token_resp.access_token)
                .is_some()
        );
        let state = server.state.read().unwrap();
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&refresh_token))
        );
        assert!(
            !state
                .revoked_tokens
                .contains_key(&access_token_digest(&token_resp.access_token))
        );
        assert!(
            !state
                .revoked_tokens
                .contains_key(&refresh_token_digest(&refresh_token))
        );
    }

    #[test]
    fn server_revoke_unknown_client_fails() {
        let server = OAuthServer::with_defaults();
        let err = server.revoke("some-token", "unknown", None).unwrap_err();
        assert_eq!(err.error_code(), "invalid_client");
    }

    // ========================================
    // Unregister edge cases
    // ========================================

    #[test]
    fn server_unregister_client_removes_auth_codes() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Create an auth code
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Verify code exists
        {
            let state = server.state.read().unwrap();
            assert!(
                state
                    .authorization_codes
                    .contains_key(&authorization_code_digest(&code))
            );
        }

        // Unregister client
        server.unregister_client("c1").unwrap();

        // Auth code should be removed
        {
            let state = server.state.read().unwrap();
            assert!(
                !state
                    .authorization_codes
                    .contains_key(&authorization_code_digest(&code))
            );
        }
    }

    // ========================================
    // Server misc
    // ========================================

    #[test]
    fn server_with_defaults_is_valid() {
        let server = OAuthServer::with_defaults();
        assert_eq!(server.config().issuer, "https://fastmcp.invalid/");
        assert!(server.config().allow_public_clients);
    }

    #[test]
    fn server_get_client_none_for_unknown() {
        let server = OAuthServer::with_defaults();
        assert!(server.get_client("nonexistent").is_none());
    }

    #[test]
    fn server_validate_access_token_after_revoke() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );

        assert!(server.validate_access_token(&resp.access_token).is_some());
        server.revoke(&resp.access_token, "c1", None).unwrap();
        assert!(server.validate_access_token(&resp.access_token).is_none());
    }

    #[test]
    fn token_verifier_claims_contain_client_id_and_issuer_but_no_false_iat() {
        let server = Arc::new(OAuthServer::with_defaults());
        let client = OAuthClient::builder("my-app")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            server.as_ref(),
            "my-app",
            "http://127.0.0.1/cb",
            &["read"],
            "user42",
        );

        let verifier = server.token_verifier(configured_resource(TEST_MCP_RESOURCE));
        let cx = asupersync::Cx::for_testing();
        let mcp_ctx = McpContext::new(cx, 1);
        let auth_request = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let access = AccessToken {
            scheme: "Bearer".to_string(),
            token: token_resp.access_token,
        };
        let auth = verifier.verify(&mcp_ctx, auth_request, &access).unwrap();

        // Claims should include client_id and issuer
        let claims = auth.claims.unwrap();
        assert_eq!(claims["client_id"], "my-app");
        assert_eq!(claims["iss"], "https://fastmcp.invalid/");
        assert!(claims.get("iat").is_none());
    }

    #[test]
    fn token_verifier_uses_client_id_as_display_subject_when_grant_has_no_subject() {
        let server = Arc::new(OAuthServer::with_defaults());
        server
            .register_client(bounded_test_client("service-client"))
            .unwrap();
        let token_resp = server
            .issue_tokens_with_draw(
                "service-client",
                &[],
                Some(TEST_MCP_RESOURCE),
                None,
                draw_security_identifier,
            )
            .unwrap();
        let verifier = server.token_verifier(configured_resource(TEST_MCP_RESOURCE));
        let cx = asupersync::Cx::for_testing();
        let mcp_ctx = McpContext::new(cx, 1);
        let auth = verifier
            .verify(
                &mcp_ctx,
                AuthRequest {
                    method: "test",
                    params: None,
                    transport_authorization: None,
                    request_id: 1,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: token_resp.access_token,
                },
            )
            .unwrap();

        assert_eq!(auth.subject.as_deref(), Some("service-client"));
        assert_eq!(auth.claims.as_ref().unwrap()["client_id"], "service-client");
        assert!(auth.claims.as_ref().unwrap()["grant_subject"].is_null());
        assert!(auth.session_owner().is_some());
    }

    #[test]
    fn oauth_session_owner_frames_every_identity_namespace() {
        let issuer = "https://issuer.example/";
        let epoch = test_registration_epoch(1);
        let stable = oauth_session_owner(issuer, "service-client", epoch, Some("subject"), None)
            .expect("bounded owner");

        assert_eq!(
            stable,
            oauth_session_owner(issuer, "service-client", epoch, Some("subject"), None)
                .expect("stable owner")
        );
        assert_ne!(
            oauth_session_owner(issuer, "service-client", epoch, None, None).expect("client owner"),
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("service-client"),
                None
            )
            .expect("subject owner")
        );
        assert_ne!(
            stable,
            oauth_session_owner(
                "https://other-issuer.example/",
                "service-client",
                epoch,
                Some("subject"),
                None,
            )
            .expect("different issuer owner")
        );
        assert_ne!(
            stable,
            oauth_session_owner(issuer, "other-client", epoch, Some("subject"), None)
                .expect("different client owner")
        );
        assert_ne!(
            stable,
            oauth_session_owner(
                issuer,
                "service-client",
                test_registration_epoch(2),
                Some("subject"),
                None,
            )
            .expect("different registration owner")
        );
        assert_ne!(
            stable,
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("subject"),
                Some("https://mcp.example.com/res-a"),
            )
            .expect("different resource owner")
        );
        assert_ne!(
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("subject"),
                Some("https://mcp.example.com/res-a"),
            )
            .expect("resource a"),
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("subject"),
                Some("https://mcp.example.com/res-b"),
            )
            .expect("resource b")
        );
        assert!(
            oauth_session_owner(issuer, "service-client", epoch, Some("subject"), Some(""),)
                .is_err()
        );
        assert!(
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("subject"),
                Some(&"x".repeat(MAX_OAUTH_RESOURCE_BYTES + 1)),
            )
            .is_err()
        );

        let first = AuthContext::with_subject("same-display").with_session_owner(stable);
        let second = AuthContext::with_subject("same-display").with_session_owner(
            oauth_session_owner(
                issuer,
                "service-client",
                test_registration_epoch(2),
                Some("subject"),
                None,
            )
            .expect("second registration owner"),
        );
        let third = AuthContext::with_subject("same-display").with_session_owner(
            oauth_session_owner(
                issuer,
                "service-client",
                epoch,
                Some("subject"),
                Some("https://mcp.example.com/res-a"),
            )
            .expect("third resource owner"),
        );
        assert_ne!(
            crate::auth::principal_fingerprint(Some(&first)).expect("first fingerprint"),
            crate::auth::principal_fingerprint(Some(&second)).expect("second fingerprint")
        );
        assert_ne!(
            crate::auth::principal_fingerprint(Some(&first)).expect("first fingerprint"),
            crate::auth::principal_fingerprint(Some(&third)).expect("third fingerprint")
        );
    }

    #[test]
    fn refresh_preserves_owner_and_absolute_family_deadline() {
        let server = Arc::new(OAuthServer::with_defaults());
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server
            .issue_tokens_with_draw(
                "c1",
                &[],
                Some(TEST_MCP_RESOURCE),
                Some("subject"),
                draw_security_identifier,
            )
            .unwrap();
        let first_refresh = initial.refresh_token.expect("initial refresh token");
        let family_expires_at = server
            .state
            .read()
            .unwrap()
            .refresh_tokens
            .get(&refresh_token_digest(&first_refresh))
            .expect("initial refresh metadata")
            .family_expires_at;
        let cx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let request = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let verifier = server.token_verifier(configured_resource(TEST_MCP_RESOURCE));
        let initial_auth = verifier
            .verify(
                &cx,
                request,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: initial.access_token,
                },
            )
            .expect("initial token verifies");

        let rotated = server
            .token(&bounded_refresh_request("c1", &first_refresh))
            .expect("refresh rotation");
        let rotated_refresh = rotated.refresh_token.expect("rotated refresh token");
        let rotated_auth = verifier
            .verify(
                &cx,
                request,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: rotated.access_token,
                },
            )
            .expect("rotated token verifies");

        assert_eq!(initial_auth.session_owner(), rotated_auth.session_owner());
        assert_eq!(
            crate::auth::principal_fingerprint(Some(&initial_auth)).expect("initial fingerprint"),
            crate::auth::principal_fingerprint(Some(&rotated_auth)).expect("rotated fingerprint")
        );
        let principal_binding = crate::session::SessionPrincipalBinding::default();
        let initial_fp =
            crate::auth::principal_fingerprint(Some(&initial_auth)).expect("initial fingerprint");
        assert!(principal_binding.bind_or_verify(initial_fp));
        let rotated_fp =
            crate::auth::principal_fingerprint(Some(&rotated_auth)).expect("rotated fingerprint");
        assert!(principal_binding.bind_or_verify(rotated_fp));
        let state = server.state.read().unwrap();
        let stored = state
            .refresh_tokens
            .get(&refresh_token_digest(&rotated_refresh))
            .expect("rotated refresh metadata");
        assert_eq!(stored.family_expires_at, family_expires_at);
        assert_eq!(stored.expires_at, family_expires_at);
    }

    #[test]
    fn refresh_clamps_access_credential_to_remaining_family_lifetime() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], Some("subject")).unwrap();
        let refresh = initial.refresh_token.expect("initial refresh token");
        let refresh_digest = refresh_token_digest(&refresh);
        let family_expires_at = Instant::now()
            .checked_add(Duration::from_mins(5))
            .expect("test family deadline");
        {
            let mut state = server.state.write().unwrap();
            state
                .refresh_tokens
                .get_mut(&refresh_digest)
                .expect("stored refresh token")
                .family_expires_at = family_expires_at;
        }

        let rotated = server
            .token(&bounded_refresh_request("c1", &refresh))
            .expect("refresh rotation within the shortened family");

        let state = server.state.read().unwrap();
        let access = state
            .access_tokens
            .get(&access_token_digest(&rotated.access_token))
            .expect("rotated access metadata");
        assert_eq!(access.metadata.expires_at, family_expires_at);
        assert_eq!(access.family_expires_at, family_expires_at);
        assert_eq!(
            rotated.expires_in,
            access
                .metadata
                .expires_at
                .saturating_duration_since(access.metadata.issued_at)
                .as_secs()
        );
        assert!(rotated.expires_in > 0);
        assert!(rotated.expires_in <= 5 * 60);
        let rotated_refresh = rotated.refresh_token.expect("rotated refresh token");
        let refresh = state
            .refresh_tokens
            .get(&refresh_token_digest(&rotated_refresh))
            .expect("rotated refresh metadata");
        assert_eq!(refresh.metadata.expires_at, family_expires_at);
        assert_eq!(refresh.family_expires_at, family_expires_at);
    }

    #[test]
    fn same_client_id_reregistration_receives_a_new_session_owner() {
        let server = Arc::new(OAuthServer::with_defaults());
        server
            .register_client(bounded_test_client("service-client"))
            .unwrap();
        let old = server
            .issue_tokens_with_draw(
                "service-client",
                &[],
                Some(TEST_MCP_RESOURCE),
                None,
                draw_security_identifier,
            )
            .unwrap();
        let verifier = server.token_verifier(configured_resource(TEST_MCP_RESOURCE));
        let cx = McpContext::new(asupersync::Cx::for_testing(), 1);
        let request = AuthRequest {
            method: "test",
            params: None,
            transport_authorization: None,
            request_id: 1,
        };
        let old_auth = verifier
            .verify(
                &cx,
                request,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: old.access_token,
                },
            )
            .expect("old registration token verifies");

        server.unregister_client("service-client").unwrap();
        server
            .register_client(bounded_test_client("service-client"))
            .unwrap();
        let fresh = server
            .issue_tokens_with_draw(
                "service-client",
                &[],
                Some(TEST_MCP_RESOURCE),
                None,
                draw_security_identifier,
            )
            .unwrap();
        let fresh_auth = verifier
            .verify(
                &cx,
                request,
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: fresh.access_token,
                },
            )
            .expect("fresh registration token verifies");

        assert_eq!(old_auth.subject, fresh_auth.subject);
        assert_ne!(old_auth.session_owner(), fresh_auth.session_owner());
        assert_ne!(
            crate::auth::principal_fingerprint(Some(&old_auth)).expect("old fingerprint"),
            crate::auth::principal_fingerprint(Some(&fresh_auth)).expect("fresh fingerprint")
        );
    }

    #[test]
    fn oauth_token_expires_in_secs_positive() {
        let token = OAuthToken {
            token: String::new(),
            token_type: TokenType::Bearer,
            client_id: "c".to_string(),
            scopes: vec![],
            resource: None,
            issued_at: Instant::now(),
            expires_at: Instant::now()
                .checked_add(Duration::from_secs(3600))
                .expect("test deadline"),
            subject: None,
            is_refresh_token: false,
        };
        // Should be > 0 since it expires in the future
        assert!(token.expires_in_secs() > 0);
    }

    #[test]
    fn server_refresh_token_confidential_client_auth_fails() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .secret("correct-secret")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        // Authorization does not authenticate the client; token exchange does.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        let (code, _) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec!["read".to_string()],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(verifier).unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        // Token exchange with correct secret
        let token_resp = server
            .token(&TokenRequest {
                grant_type: "authorization_code".to_string(),
                code: Some(code),
                redirect_uri: Some("http://127.0.0.1/cb".to_string()),
                client_id: "c1".to_string(),
                client_secret: Some("correct-secret".to_string()),
                code_verifier: Some(verifier.to_string()),
                refresh_token: None,
                scopes: None,
                resource: None,
            })
            .unwrap();

        let refresh = token_resp.refresh_token.unwrap();

        // Refresh with wrong secret
        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: Some("wrong-secret".to_string()),
                code_verifier: None,
                refresh_token: Some(refresh.clone()),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(err.error_code(), "invalid_client");
        assert_eq!(err.description(), OAUTH_CLIENT_AUTHENTICATION_ERROR);
        assert!(!err.description().contains("wrong-secret"));

        let one_past_secret = "x".repeat(MAX_OAUTH_CLIENT_CREDENTIAL_BYTES + 1);
        let oversized_err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: Some(one_past_secret.clone()),
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        assert_eq!(oversized_err.error_code(), "invalid_client");
        assert_eq!(
            oversized_err.description(),
            OAUTH_CLIENT_AUTHENTICATION_ERROR
        );
        assert_eq!(oversized_err.description(), err.description());
        assert!(!oversized_err.description().contains(&one_past_secret));
    }

    #[test]
    fn code_challenge_method_parse_unknown() {
        assert!(CodeChallengeMethod::parse("sha512").is_none());
        assert!(CodeChallengeMethod::parse("").is_none());
    }

    #[test]
    fn constant_time_eq_different_lengths() {
        assert!(!constant_time_eq("short", "longer_string"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn constant_time_eq_empty_strings() {
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn loopback_match_rejects_different_host_variants() {
        assert!(!loopback_match(
            "http://localhost:3000/cb",
            "http://127.0.0.1:8080/cb"
        ));
        assert!(!loopback_match(
            "http://127.0.0.1:3000/cb",
            "http://[::1]:9000/cb"
        ));
    }

    #[test]
    fn url_encode_empty_and_unicode() {
        assert_eq!(url_encode(""), "");
        // Unicode bytes get percent-encoded
        let encoded = url_encode("ü");
        assert!(encoded.contains('%'));
    }

    // ========================================
    // Additional coverage — uncovered paths
    // ========================================

    #[test]
    fn server_revoke_confidential_client_wrong_secret() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .secret("correct")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let err = server.revoke("any-token", "c1", Some("wrong")).unwrap_err();
        assert_eq!(err.error_code(), "invalid_client");
    }

    #[test]
    fn oauth_auth_01_a_valid_refresh_rotation_authenticates_and_reuses_grant() {
        let server = Arc::new(OAuthServer::with_defaults());
        let client = OAuthClient::builder("c1")
            .secret("correct")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        let initial = server
            .issue_tokens_with_draw(
                "c1",
                &[],
                Some(TEST_MCP_RESOURCE),
                Some("subject"),
                draw_security_identifier,
            )
            .unwrap();
        let refresh = initial.refresh_token.expect("refresh token");

        let mut request = bounded_refresh_request("c1", &refresh);
        request.client_secret = Some("correct".to_string());
        let rotated = server
            .token(&request)
            .expect("valid client authentication must rotate the refresh grant");
        let auth = server
            .token_verifier(configured_resource(TEST_MCP_RESOURCE))
            .verify(
                &McpContext::new(asupersync::Cx::for_testing(), 1),
                AuthRequest {
                    method: "tools/call",
                    params: None,
                    transport_authorization: None,
                    request_id: 1,
                },
                &AccessToken {
                    scheme: "Bearer".to_string(),
                    token: rotated.access_token,
                },
            )
            .expect("rotated access token must remain usable");

        assert_eq!(auth.subject.as_deref(), Some("subject"));
    }

    #[test]
    fn oauth_auth_01_a_wrong_client_refresh_denial_preserves_state_before_valid_reuse() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .secret("correct")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        let initial = server.issue_tokens("c1", &[], Some("subject")).unwrap();
        let refresh = initial.refresh_token.expect("refresh token");
        let cleanup_canary = insert_expired_authorization_code_cleanup_canary(&server, "c1");
        let before = server.stats();

        let mut denied = bounded_refresh_request("c1", &refresh);
        denied.client_secret = Some("wrong".to_string());
        let error = server
            .token(&denied)
            .expect_err("wrong client authentication must deny refresh");
        assert!(matches!(error, OAuthError::InvalidClient(_)));
        assert_oauth_stats_unchanged(&before, &server.stats());
        {
            let state = server.state.read().unwrap();
            assert!(state.authorization_codes.contains_key(&cleanup_canary));
            assert!(
                state
                    .refresh_tokens
                    .contains_key(&refresh_token_digest(&refresh))
            );
        }

        let mut valid = bounded_refresh_request("c1", &refresh);
        valid.client_secret = Some("correct".to_string());
        let rotated = server
            .token(&valid)
            .expect("the unchanged refresh remains reusable after denial");
        assert!(
            server
                .validate_access_token(&rotated.access_token)
                .is_some()
        );
    }

    #[test]
    fn oauth_auth_01_a_wrong_client_revoke_denial_preserves_state_and_token() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .secret("correct")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();
        let issued = server.issue_tokens("c1", &[], Some("subject")).unwrap();
        let cleanup_canary = insert_expired_authorization_code_cleanup_canary(&server, "c1");
        let before = server.stats();

        let error = server
            .revoke(&issued.access_token, "c1", Some("wrong"))
            .expect_err("wrong client authentication must deny revocation");
        assert!(matches!(error, OAuthError::InvalidClient(_)));
        assert_oauth_stats_unchanged(&before, &server.stats());
        assert!(server.validate_access_token(&issued.access_token).is_some());
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&cleanup_canary)
        );

        server
            .revoke(&issued.access_token, "c1", Some("correct"))
            .expect("valid client authentication must revoke the token");
        assert!(server.validate_access_token(&issued.access_token).is_none());
    }

    #[test]
    fn server_validate_access_token_expired_returns_none() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );

        {
            let mut state = server.state.write().unwrap();
            let stored = state
                .access_tokens
                .get_mut(&access_token_digest(&resp.access_token))
                .expect("stored access token");
            stored.metadata.expires_at = Instant::now();
        }
        assert!(server.validate_access_token(&resp.access_token).is_none());
    }

    #[test]
    fn server_authorize_without_state_omits_state_from_redirect() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let (_code, redirect) = server
            .authorize(&AuthorizationRequest {
                response_type: "code".to_string(),
                client_id: "c1".to_string(),
                redirect_uri: "http://127.0.0.1/cb".to_string(),
                scopes: vec![],
                resource: None,
                state: None,
                code_challenge: compute_s256_challenge(
                    "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk",
                )
                .unwrap(),
                code_challenge_method: CodeChallengeMethod::S256,
            })
            .unwrap();

        assert!(redirect.contains("code="));
        assert!(!redirect.contains("state="));
    }

    #[test]
    fn server_refresh_token_client_deleted_after_issue_fails_authentication_first() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let token_resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let refresh = token_resp.refresh_token.unwrap();

        // Delete client, then try to refresh
        server.unregister_client("c1").unwrap();

        let err = server
            .token(&TokenRequest {
                grant_type: "refresh_token".to_string(),
                code: None,
                redirect_uri: None,
                client_id: "c1".to_string(),
                client_secret: None,
                code_verifier: None,
                refresh_token: Some(refresh),
                scopes: None,
                resource: None,
            })
            .unwrap_err();

        // Client authentication deliberately precedes the revoked-grant
        // lookup, so deleted and never-registered clients are indistinguishable.
        assert_eq!(err.error_code(), "invalid_client");
        assert_eq!(err.description(), OAUTH_CLIENT_AUTHENTICATION_ERROR);
    }

    #[test]
    fn server_issue_tokens_empty_scopes_returns_no_scope() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let resp =
            issue_access_token_via_auth_code(&server, "c1", "http://127.0.0.1/cb", &[], "user1");

        assert!(resp.scope.is_none());
    }

    #[test]
    fn server_revoke_refresh_token_specifically() {
        let server = OAuthServer::with_defaults();
        let client = OAuthClient::builder("c1")
            .redirect_uri("http://127.0.0.1/cb")
            .scope("read")
            .build()
            .unwrap();
        server.register_client(client).unwrap();

        let resp = issue_access_token_via_auth_code(
            &server,
            "c1",
            "http://127.0.0.1/cb",
            &["read"],
            "user1",
        );
        let access = resp.access_token.clone();
        let refresh = resp.refresh_token.unwrap();

        // Revoke the refresh token specifically
        server.revoke(&refresh, "c1", None).unwrap();
        assert!(server.validate_access_token(&access).is_none());

        // Verify it's in revoked set
        {
            let state = server.state.read().unwrap();
            assert!(
                state
                    .revoked_tokens
                    .contains_key(&refresh_token_digest(&refresh))
            );
        }
    }

    #[test]
    fn refresh_revocation_cascades_only_within_the_selected_grant_family() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let first = server.issue_tokens("c1", &[], None).unwrap();
        let second = server.issue_tokens("c1", &[], None).unwrap();
        let first_refresh = first.refresh_token.expect("first refresh token");
        let second_refresh = second.refresh_token.expect("second refresh token");
        let state = server.state.read().unwrap();
        let first_grant = state
            .refresh_tokens
            .get(&refresh_token_digest(&first_refresh))
            .expect("first refresh metadata")
            .grant_id;
        let second_grant = state
            .refresh_tokens
            .get(&refresh_token_digest(&second_refresh))
            .expect("second refresh metadata")
            .grant_id;
        assert_ne!(first_grant, second_grant);
        drop(state);

        server.revoke(&first_refresh, "c1", None).unwrap();

        assert!(server.validate_access_token(&first.access_token).is_none());
        assert!(server.validate_access_token(&second.access_token).is_some());
        let state = server.state.read().unwrap();
        assert!(
            !state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&first_refresh))
        );
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&second_refresh))
        );
        assert!(
            state
                .access_tokens
                .values()
                .all(|token| token.grant_id != first_grant)
        );
        assert!(
            state
                .refresh_tokens
                .values()
                .all(|token| token.grant_id != first_grant)
        );
    }

    #[test]
    fn issuer_validation_accepts_exact_bound_and_canonical_https_urls() {
        let exact = exact_ascii_value("https://issuer.example/", MAX_OAUTH_ISSUER_BYTES);
        assert!(
            OAuthServer::try_new(OAuthServerConfig {
                issuer: exact,
                ..OAuthServerConfig::default()
            })
            .is_ok()
        );
        for issuer in [
            "https://issuer.example",
            "https://issuer.example/",
            "https://issuer.example/oauth",
        ] {
            assert!(
                OAuthServer::try_new(OAuthServerConfig {
                    issuer: issuer.to_string(),
                    ..OAuthServerConfig::default()
                })
                .is_ok(),
                "canonical HTTPS issuer should be admitted: {issuer}"
            );
        }
    }

    #[test]
    fn pathless_authority_is_an_issuer_spelling_but_never_a_redirect_uri() {
        // RFC 8414's own issuer form: `Url` serializes it with a root path.
        let server = Arc::new(
            OAuthServer::try_new(OAuthServerConfig {
                issuer: "https://issuer.example".to_string(),
                ..OAuthServerConfig::default()
            })
            .unwrap(),
        );
        let routes = OAuthHttpRoutes::new(server, "https://issuer.example").unwrap();
        assert_eq!(routes.token_path(), "/token");
        // Only the appended root path is forgiven; any other normalisation of
        // the same path-less authority is still refused.
        for issuer in ["https://ISSUER.example", "https://issuer.example:443"] {
            assert!(validate_oauth_issuer(issuer).is_err(), "{issuer}");
        }
        // Redirect URIs stay byte-exact, so the identical spelling is refused.
        assert!(parse_redirect_uri("https://app.example").is_none());
        assert!(parse_redirect_uri("https://app.example/").is_some());
    }

    #[test]
    fn issuer_validation_rejects_one_past_and_unsafe_urls_with_fixed_error() {
        let one_past = exact_ascii_value("https://issuer.example/", MAX_OAUTH_ISSUER_BYTES + 1);
        let invalid = [
            one_past.as_str(),
            "fastmcp",
            "http://issuer.example",
            "http://localhost:8080",
            "http://127.0.0.1:8080/oauth",
            "http://[::1]:8080/oauth",
            "https:issuer.example",
            "https://ISSUER.example/",
            "https://issuer.example:443/",
            "https://issuer.example/a/../",
            "https://user:password@issuer.example",
            "https://@issuer.example",
            "https://issuer.example/#fragment",
            "https://issuer.example/?tenant=one",
            "javascript:alert(1)",
            "https://issuer.example/\r\nheader",
        ];

        for issuer in invalid {
            let error = match OAuthServer::try_new(OAuthServerConfig {
                issuer: issuer.to_string(),
                ..OAuthServerConfig::default()
            }) {
                Ok(_) => panic!("unsafe issuer must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.description(), OAUTH_ISSUER_ERROR);
            assert!(!error.description().contains(issuer));
        }
    }

    #[test]
    fn config_accepts_each_exact_retention_hard_ceiling_in_isolation() {
        let valid = [
            (
                "max_clients",
                OAuthServerConfig {
                    max_clients: HARD_MAX_OAUTH_CLIENTS,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes",
                OAuthServerConfig {
                    max_authorization_codes: HARD_MAX_AUTHORIZATION_CODES,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes_per_client",
                OAuthServerConfig {
                    max_authorization_codes_per_client: HARD_MAX_AUTHORIZATION_CODES_PER_CLIENT,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens",
                OAuthServerConfig {
                    max_access_tokens: HARD_MAX_ACCESS_TOKENS,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens_per_client",
                OAuthServerConfig {
                    max_access_tokens_per_client: HARD_MAX_ACCESS_TOKENS_PER_CLIENT,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens",
                OAuthServerConfig {
                    max_refresh_tokens: HARD_MAX_REFRESH_TOKENS,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens_per_client",
                OAuthServerConfig {
                    max_refresh_tokens_per_client: HARD_MAX_REFRESH_TOKENS_PER_CLIENT,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones",
                OAuthServerConfig {
                    max_revocation_tombstones: HARD_MAX_REVOCATION_TOMBSTONES,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones_per_client",
                OAuthServerConfig {
                    max_revocation_tombstones_per_client: HARD_MAX_REVOCATION_TOMBSTONES_PER_CLIENT,
                    ..OAuthServerConfig::default()
                },
            ),
        ];

        for (field, config) in valid {
            let result = config.validate();
            assert!(
                result.is_ok(),
                "exact {field} hard ceiling rejected: {result:?}"
            );
        }
    }

    #[test]
    fn config_rejects_one_past_every_retention_hard_ceiling_before_mutation() {
        let invalid = [
            (
                "max_clients",
                OAuthServerConfig {
                    max_clients: HARD_MAX_OAUTH_CLIENTS + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes",
                OAuthServerConfig {
                    max_authorization_codes: HARD_MAX_AUTHORIZATION_CODES + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes_per_client",
                OAuthServerConfig {
                    max_authorization_codes_per_client: HARD_MAX_AUTHORIZATION_CODES_PER_CLIENT + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens",
                OAuthServerConfig {
                    max_access_tokens: HARD_MAX_ACCESS_TOKENS + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens_per_client",
                OAuthServerConfig {
                    max_access_tokens_per_client: HARD_MAX_ACCESS_TOKENS_PER_CLIENT + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens",
                OAuthServerConfig {
                    max_refresh_tokens: HARD_MAX_REFRESH_TOKENS + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens_per_client",
                OAuthServerConfig {
                    max_refresh_tokens_per_client: HARD_MAX_REFRESH_TOKENS_PER_CLIENT + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones",
                OAuthServerConfig {
                    max_revocation_tombstones: HARD_MAX_REVOCATION_TOMBSTONES + 1,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones_per_client",
                OAuthServerConfig {
                    max_revocation_tombstones_per_client: HARD_MAX_REVOCATION_TOMBSTONES_PER_CLIENT
                        + 1,
                    ..OAuthServerConfig::default()
                },
            ),
        ];

        for (field, config) in invalid {
            let error = config
                .validate()
                .expect_err("one-past-hard-ceiling config must fail closed");
            assert!(matches!(&error, OAuthError::ServerError(_)));
            assert!(error.description().contains(field));
            assert!(error.description().contains("hard ceiling"));
            assert!(matches!(
                OAuthServer::try_new(config.clone()),
                Err(OAuthError::ServerError(_))
            ));

            let server = OAuthServer::new(config);
            assert!(matches!(
                server.register_client(bounded_test_client("c1")),
                Err(OAuthError::ServerError(_))
            ));
            assert_eq!(server.stats().clients, 0);
        }
    }

    #[test]
    fn config_enforces_checked_aggregate_retention_boundary() {
        let mut exact = OAuthServerConfig {
            max_access_tokens: HARD_MAX_ACCESS_TOKENS,
            ..OAuthServerConfig::default()
        };
        let subtotal_without_tombstones = exact
            .max_clients
            .checked_add(exact.max_authorization_codes)
            .and_then(|total| total.checked_add(exact.max_access_tokens))
            .and_then(|total| total.checked_add(exact.max_refresh_tokens))
            .expect("test subtotal is representable");
        exact.max_revocation_tombstones = HARD_MAX_OAUTH_RETAINED_ENTRIES
            .checked_sub(subtotal_without_tombstones)
            .expect("aggregate ceiling admits the default retention profile");
        assert!(exact.max_revocation_tombstones <= HARD_MAX_REVOCATION_TOMBSTONES);
        assert_eq!(
            exact
                .checked_global_retention_limit()
                .expect("exact aggregate is representable"),
            HARD_MAX_OAUTH_RETAINED_ENTRIES
        );
        exact
            .validate()
            .expect("exact aggregate retention ceiling is admitted");

        let mut one_past = exact;
        one_past.max_revocation_tombstones = one_past
            .max_revocation_tombstones
            .checked_add(1)
            .expect("one-past test value is representable");
        let error = one_past
            .validate()
            .expect_err("one-past aggregate retention ceiling must fail closed");
        assert!(matches!(&error, OAuthError::ServerError(_)));
        assert!(error.description().contains("aggregate retained-state"));
        assert!(error.description().contains("hard ceiling"));
        assert!(matches!(
            OAuthServer::try_new(one_past.clone()),
            Err(OAuthError::ServerError(_))
        ));
        let server = OAuthServer::new(one_past);
        assert!(matches!(
            server.register_client(bounded_test_client("c1")),
            Err(OAuthError::ServerError(_))
        ));
        assert_eq!(server.stats().clients, 0);

        let unrepresentable = OAuthServerConfig {
            max_clients: usize::MAX,
            ..OAuthServerConfig::default()
        };
        let error = unrepresentable
            .checked_global_retention_limit()
            .expect_err("aggregate arithmetic must reject usize overflow");
        assert!(error.description().contains("not representable"));
    }

    #[test]
    fn config_rejects_every_zero_state_limit() {
        let invalid = [
            (
                "max_clients",
                OAuthServerConfig {
                    max_clients: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes",
                OAuthServerConfig {
                    max_authorization_codes: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_authorization_codes_per_client",
                OAuthServerConfig {
                    max_authorization_codes_per_client: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens",
                OAuthServerConfig {
                    max_access_tokens: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_access_tokens_per_client",
                OAuthServerConfig {
                    max_access_tokens_per_client: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens",
                OAuthServerConfig {
                    max_refresh_tokens: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_refresh_tokens_per_client",
                OAuthServerConfig {
                    max_refresh_tokens_per_client: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones",
                OAuthServerConfig {
                    max_revocation_tombstones: 0,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "max_revocation_tombstones_per_client",
                OAuthServerConfig {
                    max_revocation_tombstones_per_client: 0,
                    ..OAuthServerConfig::default()
                },
            ),
        ];

        for (field, config) in invalid {
            let error = config.validate().expect_err("zero cap must fail closed");
            assert!(matches!(&error, OAuthError::ServerError(_)));
            assert!(error.description().contains(field));
            assert!(matches!(
                OAuthServer::try_new(config.clone()),
                Err(OAuthError::ServerError(_))
            ));
            let server = OAuthServer::new(config);
            assert!(matches!(
                server.register_client(bounded_test_client("c1")),
                Err(OAuthError::ServerError(_))
            ));
            assert_eq!(server.stats().clients, 0);
        }
    }

    #[test]
    fn unrepresentable_lifetimes_fail_eager_and_lazy_construction() {
        let invalid = [
            (
                "access_token_lifetime",
                OAuthServerConfig {
                    access_token_lifetime: Duration::MAX,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "refresh_token_lifetime",
                OAuthServerConfig {
                    refresh_token_lifetime: Duration::MAX,
                    ..OAuthServerConfig::default()
                },
            ),
            (
                "authorization_code_lifetime",
                OAuthServerConfig {
                    authorization_code_lifetime: Duration::MAX,
                    ..OAuthServerConfig::default()
                },
            ),
        ];

        for (field, config) in invalid {
            let error = config
                .validate()
                .expect_err("Duration::MAX must not form an Instant deadline");
            assert!(matches!(&error, OAuthError::ServerError(_)));
            assert!(error.description().contains(field));
            assert!(matches!(
                OAuthServer::try_new(config.clone()),
                Err(OAuthError::ServerError(_))
            ));

            // The infallible constructor is retained for API compatibility,
            // but every mutation still validates before changing state.
            let server = OAuthServer::new(config);
            assert!(matches!(
                server.register_client(bounded_test_client("c1")),
                Err(OAuthError::ServerError(_))
            ));
            assert_eq!(server.stats().clients, 0);
        }
    }

    #[test]
    fn registration_rejects_exact_registered_redirect_with_fragment() {
        let server = OAuthServer::with_defaults();
        let mut client = OAuthClient::builder("fragmented")
            .redirect_uri("https://example.com/callback")
            .build()
            .unwrap();
        client.redirect_uris = vec!["https://example.com/callback#fragment".to_string()];
        assert!(!client.validate_redirect_uri(&client.redirect_uris[0]));

        let error = server
            .register_client(client)
            .expect_err("fragment-bearing registration must fail closed");
        assert!(matches!(&error, OAuthError::InvalidRequest(_)));
        assert_eq!(error.description(), OAUTH_CLIENT_REDIRECT_VALUE_ERROR);
        assert!(server.state.read().unwrap().clients.is_empty());
    }

    #[test]
    fn client_capacity_accepts_exact_limit_and_rejects_next() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_clients: 2,
            ..OAuthServerConfig::default()
        });

        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();
        let error = server
            .register_client(bounded_test_client("c3"))
            .expect_err("third client must exceed exact cap");

        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        assert_eq!(server.stats().clients, 2);

        server.unregister_client("c1").unwrap();
        server.register_client(bounded_test_client("c3")).unwrap();
        assert_eq!(server.stats().clients, 2);
    }

    #[test]
    fn authorization_code_global_capacity_is_exact_and_atomic() {
        let server = configured_approved_test_server(OAuthServerConfig {
            max_authorization_codes: 2,
            max_authorization_codes_per_client: 2,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        let error = server
            .authorize(&bounded_authorization_request("c2"))
            .expect_err("global authorization-code cap must reject the next code");

        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        let state = server.state.read().unwrap();
        assert_eq!(state.authorization_codes.len(), 2);
        assert_eq!(state.authorization_code_count_for_client("c1"), 2);
        assert_eq!(state.authorization_code_count_for_client("c2"), 0);
    }

    #[test]
    fn authorization_code_per_client_capacity_is_exact_and_isolated() {
        let server = configured_approved_test_server(OAuthServerConfig {
            max_authorization_codes: 3,
            max_authorization_codes_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        let error = server
            .authorize(&bounded_authorization_request("c1"))
            .expect_err("per-client authorization-code cap must reject the next code");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        server
            .authorize(&bounded_authorization_request("c2"))
            .unwrap();

        let state = server.state.read().unwrap();
        assert_eq!(state.authorization_codes.len(), 2);
        assert_eq!(state.authorization_code_count_for_client("c1"), 1);
        assert_eq!(state.authorization_code_count_for_client("c2"), 1);
    }

    #[test]
    fn access_token_global_capacity_is_exact_and_pair_atomic() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_access_tokens: 2,
            max_access_tokens_per_client: 2,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server.issue_tokens("c1", &[], None).unwrap();
        server.issue_tokens("c1", &[], None).unwrap();
        let error = server
            .issue_tokens("c2", &[], None)
            .expect_err("global access-token cap must reject the next pair");

        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 2);
        assert_eq!(state.refresh_tokens.len(), 2);
        assert_eq!(state.access_token_count_for_client("c2"), 0);
        assert_eq!(state.refresh_token_count_for_client("c2"), 0);
    }

    #[test]
    fn access_token_per_client_capacity_is_exact_and_isolated() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_access_tokens: 3,
            max_access_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server.issue_tokens("c1", &[], None).unwrap();
        let error = server
            .issue_tokens("c1", &[], None)
            .expect_err("per-client access-token cap must reject the next pair");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        server.issue_tokens("c2", &[], None).unwrap();

        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 2);
        assert_eq!(state.refresh_tokens.len(), 2);
        assert_eq!(state.access_token_count_for_client("c1"), 1);
        assert_eq!(state.access_token_count_for_client("c2"), 1);
    }

    #[test]
    fn refresh_token_global_capacity_is_exact_and_pair_atomic() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_refresh_tokens: 2,
            max_refresh_tokens_per_client: 2,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server.issue_tokens("c1", &[], None).unwrap();
        server.issue_tokens("c1", &[], None).unwrap();
        let error = server
            .issue_tokens("c2", &[], None)
            .expect_err("global refresh-token cap must reject the next pair");

        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 2);
        assert_eq!(state.refresh_tokens.len(), 2);
        assert_eq!(state.access_token_count_for_client("c2"), 0);
        assert_eq!(state.refresh_token_count_for_client("c2"), 0);
    }

    #[test]
    fn refresh_token_per_client_capacity_is_exact_and_isolated() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_refresh_tokens: 3,
            max_refresh_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        server.register_client(bounded_test_client("c2")).unwrap();

        server.issue_tokens("c1", &[], None).unwrap();
        let error = server
            .issue_tokens("c1", &[], None)
            .expect_err("per-client refresh-token cap must reject the next pair");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        server.issue_tokens("c2", &[], None).unwrap();

        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 2);
        assert_eq!(state.refresh_tokens.len(), 2);
        assert_eq!(state.refresh_token_count_for_client("c1"), 1);
        assert_eq!(state.refresh_token_count_for_client("c2"), 1);
    }

    #[test]
    fn code_exchange_capacity_failure_preserves_single_use_code_for_retry() {
        let server = configured_approved_test_server(OAuthServerConfig {
            max_access_tokens: 1,
            max_access_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let blocker = server.issue_tokens("c1", &[], None).unwrap();
        let (code, _) = server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        let request = bounded_code_exchange_request("c1", &code);

        let error = server
            .token(&request)
            .expect_err("full access-token capacity must reject exchange");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );

        server.revoke(&blocker.access_token, "c1", None).unwrap();
        server.token(&request).unwrap();
        assert!(
            !server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn code_exchange_refresh_capacity_failure_preserves_single_use_code_for_retry() {
        let server = configured_approved_test_server(OAuthServerConfig {
            max_access_tokens: 2,
            max_access_tokens_per_client: 2,
            max_refresh_tokens: 1,
            max_refresh_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let blocker = server.issue_tokens("c1", &[], None).unwrap();
        let blocker_refresh = blocker.refresh_token.expect("refresh token");
        let (code, _) = server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        let request = bounded_code_exchange_request("c1", &code);

        let error = server
            .token(&request)
            .expect_err("full refresh-token capacity must reject exchange");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        assert!(
            server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );

        server.revoke(&blocker_refresh, "c1", None).unwrap();
        server.token(&request).unwrap();
        assert!(
            !server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn code_exchange_second_draw_failure_preserves_single_use_code_for_retry() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let (code, _) = server
            .authorize(&bounded_authorization_request("c1"))
            .unwrap();
        let request = bounded_code_exchange_request("c1", &code);
        let draw_calls = std::cell::Cell::new(0);

        let result = server.token_authorization_code_with_draw(&request, || {
            let call = draw_calls.get() + 1;
            draw_calls.set(call);
            if call == 1 {
                draw_security_identifier().map_err(|_| "unexpected operating-system RNG failure")
            } else {
                Err("forced refresh-token draw failure")
            }
        });

        assert!(matches!(result, Err(OAuthError::ServerError(_))));
        assert_eq!(draw_calls.get(), 2);
        {
            let state = server.state.read().unwrap();
            assert!(
                state
                    .authorization_codes
                    .contains_key(&authorization_code_digest(&code))
            );
            assert!(state.access_tokens.is_empty());
            assert!(state.refresh_tokens.is_empty());
        }
        server.token(&request).unwrap();
        assert!(
            !server
                .state
                .read()
                .unwrap()
                .authorization_codes
                .contains_key(&authorization_code_digest(&code))
        );
    }

    #[test]
    fn refresh_capacity_failure_preserves_presented_token_for_retry() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_access_tokens: 1,
            max_access_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], None).unwrap();
        let old_access = initial.access_token;
        let old_refresh = initial.refresh_token.expect("refresh token");
        let request = bounded_refresh_request("c1", &old_refresh);

        let error = server
            .token(&request)
            .expect_err("full access-token capacity must reject refresh");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        {
            let state = server.state.read().unwrap();
            assert!(
                state
                    .refresh_tokens
                    .contains_key(&refresh_token_digest(&old_refresh))
            );
            assert!(
                !state
                    .revoked_tokens
                    .contains_key(&refresh_token_digest(&old_refresh))
            );
            assert_eq!(state.access_tokens.len(), 1);
            assert_eq!(state.refresh_tokens.len(), 1);
        }

        server.revoke(&old_access, "c1", None).unwrap();
        let retried = server.token(&request).unwrap();
        let rotated = retried.refresh_token.expect("rotated refresh token");
        assert_ne!(rotated, old_refresh);
        let state = server.state.read().unwrap();
        assert!(
            !state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&old_refresh))
        );
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&rotated))
        );
        assert!(
            state
                .revoked_tokens
                .contains_key(&refresh_token_digest(&old_refresh))
        );
    }

    #[test]
    fn successful_refresh_rotates_single_use_token_and_rejects_replay() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_refresh_tokens: 1,
            max_refresh_tokens_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], Some("subject")).unwrap();
        let initial_access = initial.access_token.clone();
        let first_refresh = initial.refresh_token.expect("refresh token");
        let grant_id = server
            .state
            .read()
            .unwrap()
            .refresh_tokens
            .get(&refresh_token_digest(&first_refresh))
            .expect("initial refresh metadata")
            .grant_id;

        let first_response = server
            .token(&bounded_refresh_request("c1", &first_refresh))
            .unwrap();
        let second_access = first_response.access_token.clone();
        let second_refresh = first_response
            .refresh_token
            .expect("successful refresh must rotate");
        assert_ne!(second_refresh, first_refresh);
        {
            let state = server.state.read().unwrap();
            assert_eq!(state.access_tokens.len(), 2);
            assert_eq!(state.refresh_tokens.len(), 1);
            assert!(
                !state
                    .refresh_tokens
                    .contains_key(&refresh_token_digest(&first_refresh))
            );
            assert!(
                state
                    .refresh_tokens
                    .contains_key(&refresh_token_digest(&second_refresh))
            );
            assert_eq!(
                state
                    .refresh_tokens
                    .get(&refresh_token_digest(&second_refresh))
                    .expect("rotated refresh metadata")
                    .grant_id,
                grant_id
            );
            assert!(
                state
                    .revoked_tokens
                    .contains_key(&refresh_token_digest(&first_refresh))
            );
        }

        let replay = server
            .token(&bounded_refresh_request("c1", &first_refresh))
            .expect_err("consumed refresh token must never be reusable");
        assert!(matches!(&replay, OAuthError::InvalidGrant(_)));
        assert_eq!(replay.description(), OAUTH_INVALID_GRANT_ERROR);
        assert!(server.validate_access_token(&initial_access).is_none());
        assert!(server.validate_access_token(&second_access).is_none());
        assert!(matches!(
            server.token(&bounded_refresh_request("c1", &second_refresh)),
            Err(OAuthError::InvalidGrant(_))
        ));
        let state = server.state.read().unwrap();
        assert!(
            state
                .access_tokens
                .values()
                .all(|token| token.grant_id != grant_id)
        );
        assert!(
            state
                .refresh_tokens
                .values()
                .all(|token| token.grant_id != grant_id)
        );
    }

    #[test]
    fn concurrent_refresh_replay_allows_one_rotation_then_revokes_its_family() {
        let server = Arc::new(OAuthServer::with_defaults());
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], None).unwrap();
        let initial_access = initial.access_token;
        let refresh = initial.refresh_token.expect("refresh token");
        let grant_id = server
            .state
            .read()
            .unwrap()
            .refresh_tokens
            .get(&refresh_token_digest(&refresh))
            .expect("initial refresh metadata")
            .grant_id;
        let barrier = Arc::new(std::sync::Barrier::new(3));

        let workers: Vec<_> = (0..2)
            .map(|_| {
                let server = Arc::clone(&server);
                let barrier = Arc::clone(&barrier);
                let refresh = refresh.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    server.token(&bounded_refresh_request("c1", &refresh))
                })
            })
            .collect();
        barrier.wait();
        let results: Vec<_> = workers
            .into_iter()
            .map(|worker| worker.join().expect("refresh worker must not panic"))
            .collect();

        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(OAuthError::InvalidGrant(_))))
                .count(),
            1
        );
        let rotated_access = results
            .into_iter()
            .find_map(Result::ok)
            .expect("one refresh rotation succeeds")
            .access_token;
        assert!(server.validate_access_token(&initial_access).is_none());
        assert!(server.validate_access_token(&rotated_access).is_none());
        let state = server.state.read().unwrap();
        assert!(
            state
                .access_tokens
                .values()
                .all(|token| token.grant_id != grant_id)
        );
        assert!(
            state
                .refresh_tokens
                .values()
                .all(|token| token.grant_id != grant_id)
        );
    }

    #[test]
    fn refresh_rotation_fails_without_evicting_a_live_replay_guard() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_revocation_tombstones: 1,
            max_revocation_tombstones_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], None).unwrap();
        let first_refresh = initial.refresh_token.expect("initial refresh token");
        let rotated = server
            .token(&bounded_refresh_request("c1", &first_refresh))
            .unwrap();
        let second_refresh = rotated.refresh_token.expect("rotated refresh token");

        let error = server
            .token(&bounded_refresh_request("c1", &second_refresh))
            .expect_err("rotation must fail closed when its replay guard cannot be retained");
        assert!(matches!(error, OAuthError::TemporarilyUnavailable(_)));
        let state = server.state.read().unwrap();
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&second_refresh))
        );
        assert!(
            state
                .revoked_tokens
                .get(&refresh_token_digest(&first_refresh))
                .is_some_and(|tombstone| tombstone.replay_guard)
        );
    }

    #[test]
    fn refresh_rotation_rejects_a_family_with_less_than_one_wire_second_left() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], None).unwrap();
        let refresh = initial.refresh_token.expect("refresh token");
        let refresh_digest = refresh_token_digest(&refresh);
        let near_deadline = Instant::now()
            .checked_add(Duration::from_millis(999))
            .expect("near family deadline");
        {
            let mut state = server.state.write().unwrap();
            let stored = state
                .refresh_tokens
                .get_mut(&refresh_digest)
                .expect("stored refresh token");
            stored.metadata.expires_at = near_deadline;
            stored.family_expires_at = near_deadline;
        }

        let error = server
            .token(&bounded_refresh_request("c1", &refresh))
            .expect_err("a zero-second wire credential must not be issued");

        assert!(matches!(error, OAuthError::InvalidGrant(_)));
        let state = server.state.read().unwrap();
        assert!(!state.revoked_tokens.contains_key(&refresh_digest));
    }

    #[test]
    fn unregister_purges_old_epoch_guards_before_same_id_reregistration() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_revocation_tombstones: 1,
            max_revocation_tombstones_per_client: 1,
            ..OAuthServerConfig::default()
        });
        server.register_client(bounded_test_client("c1")).unwrap();
        let old = server.issue_tokens("c1", &[], None).unwrap();
        let old_refresh = old.refresh_token.expect("old refresh token");
        let rotated = server
            .token(&bounded_refresh_request("c1", &old_refresh))
            .expect("first registration rotates once");
        assert_eq!(server.stats().revoked_tokens, 1);

        server.unregister_client("c1").unwrap();
        {
            let state = server.state.read().unwrap();
            assert!(state.authorization_codes.is_empty());
            assert!(state.access_tokens.is_empty());
            assert!(state.refresh_tokens.is_empty());
            assert!(state.revoked_tokens.is_empty());
        }
        server.register_client(bounded_test_client("c1")).unwrap();
        assert!(matches!(
            server.token(&bounded_refresh_request("c1", &old_refresh)),
            Err(OAuthError::InvalidGrant(_))
        ));
        let fresh = server.issue_tokens("c1", &[], None).unwrap();
        let fresh_refresh = fresh.refresh_token.expect("fresh refresh token");
        server
            .token(&bounded_refresh_request("c1", &fresh_refresh))
            .expect("fresh registration receives a fresh replay-guard budget");

        // The previous registration's most recent descendant is invalid too.
        let rotated_refresh = rotated.refresh_token.expect("rotated old refresh token");
        assert!(matches!(
            server.token(&bounded_refresh_request("c1", &rotated_refresh)),
            Err(OAuthError::InvalidGrant(_))
        ));
    }

    #[test]
    fn second_refresh_draw_failure_preserves_presented_token() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let initial = server.issue_tokens("c1", &[], None).unwrap();
        let old_refresh = initial.refresh_token.expect("refresh token");
        let request = bounded_refresh_request("c1", &old_refresh);
        let draw_calls = std::cell::Cell::new(0);

        let result = server.token_refresh_token_with_draw(&request, || {
            let call = draw_calls.get() + 1;
            draw_calls.set(call);
            if call == 1 {
                draw_security_identifier().map_err(|_| "unexpected operating-system RNG failure")
            } else {
                Err("forced replacement refresh-token draw failure")
            }
        });

        assert!(matches!(result, Err(OAuthError::ServerError(_))));
        assert_eq!(draw_calls.get(), 2);
        let state = server.state.read().unwrap();
        assert_eq!(state.access_tokens.len(), 1);
        assert_eq!(state.refresh_tokens.len(), 1);
        assert!(
            state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&old_refresh))
        );
        assert!(
            !state
                .revoked_tokens
                .contains_key(&refresh_token_digest(&old_refresh))
        );
    }

    #[test]
    fn revocation_tombstones_carry_expiry_and_obey_both_exact_caps() {
        let server = OAuthServer::new(OAuthServerConfig {
            max_revocation_tombstones: 2,
            max_revocation_tombstones_per_client: 1,
            ..OAuthServerConfig::default()
        });
        for client_id in ["c1", "c2", "c3"] {
            server
                .register_client(bounded_test_client(client_id))
                .unwrap();
        }
        let first = server.issue_tokens("c1", &[], None).unwrap();
        let second = server.issue_tokens("c2", &[], None).unwrap();
        let third = server.issue_tokens("c3", &[], None).unwrap();
        let first_refresh = first.refresh_token.expect("refresh token");
        let first_refresh_expiry = server
            .state
            .read()
            .unwrap()
            .refresh_tokens
            .get(&refresh_token_digest(&first_refresh))
            .expect("stored refresh token")
            .expires_at;

        server.revoke(&first.access_token, "c1", None).unwrap();
        server.revoke(&first_refresh, "c1", None).unwrap();
        {
            let state = server.state.read().unwrap();
            assert_eq!(state.revoked_tokens.len(), 1);
            assert_eq!(state.revocation_tombstone_count_for_client("c1"), 1);
            assert_eq!(
                state
                    .revoked_tokens
                    .get(&refresh_token_digest(&first_refresh))
                    .expect("longer-lived c1 tombstone retained")
                    .expires_at,
                first_refresh_expiry
            );
        }

        server.revoke(&second.access_token, "c2", None).unwrap();
        assert_eq!(server.stats().revoked_tokens, 2);
        server.revoke(&third.access_token, "c3", None).unwrap();

        let state = server.state.read().unwrap();
        assert_eq!(state.revoked_tokens.len(), 2);
        for client_id in ["c1", "c2", "c3"] {
            assert!(state.revocation_tombstone_count_for_client(client_id) <= 1);
        }
        assert!(
            !state
                .access_tokens
                .contains_key(&access_token_digest(&first.access_token))
        );
        assert!(
            !state
                .access_tokens
                .contains_key(&access_token_digest(&second.access_token))
        );
        assert!(
            !state
                .access_tokens
                .contains_key(&access_token_digest(&third.access_token))
        );
        assert!(
            !state
                .refresh_tokens
                .contains_key(&refresh_token_digest(&first_refresh))
        );
        drop(state);

        for access in [
            &first.access_token,
            &second.access_token,
            &third.access_token,
        ] {
            assert!(server.validate_access_token(access).is_none());
        }
        assert!(matches!(
            server.token(&bounded_refresh_request("c1", &first_refresh)),
            Err(OAuthError::InvalidGrant(_))
        ));
    }

    #[test]
    fn every_mutation_opportunistically_cleans_all_expiry_bearing_state() {
        let server = OAuthServer::with_defaults();
        server.register_client(bounded_test_client("c1")).unwrap();
        let expired_at = Instant::now();
        let issued_at = expired_at;
        let expired_code = base64url_encode(&[1_u8; 32]);
        let expired_access = base64url_encode(&[2_u8; 32]);
        let expired_refresh = base64url_encode(&[3_u8; 32]);
        let expired_revocation = base64url_encode(&[4_u8; 32]);

        {
            let mut state = server.state.write().unwrap();
            state.authorization_codes.insert(
                authorization_code_digest(&expired_code),
                AuthorizationCode {
                    client_id: "c1".to_string(),
                    redirect_uri: "http://127.0.0.1/callback".to_string(),
                    scopes: Vec::new(),
                    resource: None,
                    code_challenge: "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM".to_string(),
                    code_challenge_method: CodeChallengeMethod::S256,
                    issued_at,
                    expires_at: expired_at,
                    subject: None,
                    state: None,
                    registration_epoch: test_registration_epoch(1),
                },
            );
            state.access_tokens.insert(
                access_token_digest(&expired_access),
                StoredOAuthToken {
                    metadata: OAuthToken {
                        token: String::new(),
                        token_type: TokenType::Bearer,
                        client_id: "c1".to_string(),
                        scopes: Vec::new(),
                        resource: None,
                        issued_at,
                        expires_at: expired_at,
                        subject: None,
                        is_refresh_token: false,
                    },
                    grant_id: test_grant_id(6),
                    registration_epoch: test_registration_epoch(1),
                    family_expires_at: expired_at,
                },
            );
            state.refresh_tokens.insert(
                refresh_token_digest(&expired_refresh),
                StoredOAuthToken {
                    metadata: OAuthToken {
                        token: String::new(),
                        token_type: TokenType::Bearer,
                        client_id: "c1".to_string(),
                        scopes: Vec::new(),
                        resource: None,
                        issued_at,
                        expires_at: expired_at,
                        subject: None,
                        is_refresh_token: true,
                    },
                    grant_id: test_grant_id(6),
                    registration_epoch: test_registration_epoch(1),
                    family_expires_at: expired_at,
                },
            );
            state.revoked_tokens.insert(
                refresh_token_digest(&expired_revocation),
                RevocationTombstone {
                    client_id: "c1".to_string(),
                    grant_id: test_grant_id(6),
                    replay_guard: false,
                    expires_at: expired_at,
                },
            );
        }

        // Client registration is otherwise unrelated to credential state;
        // its write gate must still clean every expiry-bearing collection.
        server.register_client(bounded_test_client("c2")).unwrap();
        let state = server.state.read().unwrap();
        assert!(state.authorization_codes.is_empty());
        assert!(state.access_tokens.is_empty());
        assert!(state.refresh_tokens.is_empty());
        assert!(state.revoked_tokens.is_empty());
    }

    #[test]
    fn loopback_match_no_explicit_port() {
        assert!(loopback_match(
            "http://127.0.0.1/callback",
            "http://127.0.0.1:8080/callback"
        ));
        assert!(loopback_match(
            "http://127.0.0.1/callback",
            "http://127.0.0.1/callback"
        ));
    }

    #[test]
    fn oauth_crypto_ownership_and_fallbacks_are_denied() {
        let source = include_str!("oauth.rs").replace("\r\n", "\n");
        let production = source
            .split_once("\n#[cfg(test)]\nmod tests")
            .map_or(source.as_str(), |(production, _)| production);

        assert!(!production.contains("getrandom::"));
        assert!(!production.contains("sha2::"));
        assert!(!production.contains("hmac::"));
        assert!(!production.contains("generate_token(bytes"));
        assert!(production.contains("draw_security_identifier"));
        assert!(production.contains("sha256_bounded"));

        let token_helper_start = production
            .find("fn generate_token()")
            .expect("token helper marker");
        let token_helper_end = production[token_helper_start..]
            .find("/// Base64url encodes bytes")
            .map(|offset| token_helper_start + offset)
            .expect("token helper end marker");
        let token_helper = &production[token_helper_start..token_helper_end];
        for fallback in [
            "SystemTime",
            "Instant",
            "process::",
            "thread::",
            "Atomic",
            "rand::",
            "usize::MAX",
        ] {
            assert!(
                !token_helper.contains(fallback),
                "security-token fallback found: {fallback}"
            );
        }
    }
    #[test]
    fn dynamic_native_registration_retains_public_client_and_exact_profile() {
        let server = OAuthServer::with_defaults();
        let request = NativePublicClientRegistrationRequest {
            client_name: "Native app".to_string(),
            application_type: "native".to_string(),
            redirect_uris: vec![
                "http://127.0.0.1/oauth/callback".to_string(),
                "http://[::1]/oauth/callback".to_string(),
            ],
            token_endpoint_auth_method: "none".to_string(),
            grant_types: vec!["authorization_code".to_string(), "refresh_token".to_string()],
            response_types: vec!["code".to_string()],
            scope: Some("tools:read tools:write".to_string()),
        };
        let response = server.register_native_public_client(request).unwrap();
        assert_eq!(response.application_type, "native");
        assert_eq!(response.token_endpoint_auth_method, "none");
        assert_eq!(response.grant_types, ["authorization_code", "refresh_token"]);
        assert_eq!(response.response_types, ["code"]);
        assert_eq!(response.scope.as_deref(), Some("tools:read tools:write"));
        assert_eq!(response.redirect_uris.len(), 2);
        let retained = server.get_client(&response.client_id).expect("registered client");
        assert_eq!(retained.client_type, ClientType::Public);
        assert_eq!(retained.redirect_uris, response.redirect_uris);
        assert_eq!(retained.allowed_scopes.len(), 2);
    }

    #[test]
    fn dynamic_registration_refuses_nonpublic_profiles_without_state_change() {
        let server = OAuthServer::with_defaults();
        let baseline = server.list_clients().len();
        for request in [
            NativePublicClientRegistrationRequest {
                client_name: "Native app".to_string(),
                application_type: "web".to_string(),
                redirect_uris: vec!["http://127.0.0.1/oauth/callback".to_string()],
                token_endpoint_auth_method: "none".to_string(),
                grant_types: vec!["authorization_code".to_string()],
                response_types: vec!["code".to_string()],
                scope: None,
            },
            NativePublicClientRegistrationRequest {
                client_name: "Native app".to_string(),
                application_type: "native".to_string(),
                redirect_uris: vec![
                    "http://127.0.0.1/oauth/callback".to_string(),
                    "http://127.0.0.1/oauth/callback".to_string(),
                ],
                token_endpoint_auth_method: "none".to_string(),
                grant_types: vec!["authorization_code".to_string()],
                response_types: vec!["code".to_string()],
                scope: None,
            },
            NativePublicClientRegistrationRequest {
                client_name: "Native app".to_string(),
                application_type: "native".to_string(),
                redirect_uris: vec!["http://127.0.0.1/oauth/callback".to_string()],
                token_endpoint_auth_method: "client_secret_basic".to_string(),
                grant_types: vec!["authorization_code".to_string()],
                response_types: vec!["code".to_string()],
                scope: None,
            },
        ] {
            assert!(server.register_native_public_client(request).is_err());
            assert_eq!(server.list_clients().len(), baseline);
        }

        let disabled = OAuthServer::try_new(OAuthServerConfig {
            allow_public_clients: false,
            ..OAuthServerConfig::default()
        }).unwrap();
        assert!(disabled.register_native_public_client(NativePublicClientRegistrationRequest {
            client_name: "Native app".to_string(),
            application_type: "native".to_string(),
            redirect_uris: vec!["http://127.0.0.1/oauth/callback".to_string()],
            token_endpoint_auth_method: "none".to_string(),
            grant_types: vec!["authorization_code".to_string()],
            response_types: vec!["code".to_string()],
            scope: None,
        }).is_err());
        assert!(disabled.list_clients().is_empty());
    }


}
