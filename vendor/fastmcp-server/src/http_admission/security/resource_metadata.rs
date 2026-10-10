//! Explicit OAuth protected-resource metadata for the secured MCP endpoint.
//!
//! RFC 9728 publication is derived from the configured public HTTPS origin and
//! exact MCP path, never from a peer's Host, Forwarded, query or bearer token.
//! The document is public deployment configuration, not a per-principal catalog.
//! Installing it neither installs an authorization server nor grants access:
//! configure the server's token verifier for these issuers separately.
//!
//! The native secured listener and secured embedding entry serve the bounded
//! document without opening an authenticated MCP session. No issuer is fetched,
//! no dynamic metadata is accepted, and no signed-metadata claim is made.

use std::io::{self, Write};
use std::sync::Arc;

use fastmcp_core::CanonicalHttpUrl;
use fastmcp_transport::http::HttpResponse;
use serde_json::json;

use super::{CorsResponseHeaders, HttpSecurityError, HttpSecurityPolicy, is_token};

const WELL_KNOWN: &str = "/.well-known/oauth-protected-resource";
const MAX_ISSUERS: usize = 32;
const MAX_ISSUER_BYTES: usize = 2048;
const MAX_ISSUER_SET_BYTES: usize = 16 * 1024;
const MAX_SCOPES: usize = 256;
const MAX_SCOPE_BYTES: usize = 256;
const MAX_SCOPE_SET_BYTES: usize = 16 * 1024;
const MAX_RESOURCE_BYTES: usize = 8192;
const MAX_DOCUMENT_BYTES: usize = 64 * 1024;
const MAX_CHALLENGE_BYTES: usize = 16 * 1024;

/// Public, immutable deployment metadata. At least one explicitly configured
/// HTTPS issuer is required. Issuer identifiers are preserved byte-for-byte;
/// URL-parser repairs, userinfo, query, fragment and duplicate issuers are refused.
/// Empty scope lists are omitted rather than encoded as an empty metadata member.
#[derive(Clone)]
pub struct ProtectedResourceMetadata {
    issuers: Vec<String>,
    scopes: Vec<String>,
    name: Option<String>,
}

impl std::fmt::Debug for ProtectedResourceMetadata {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProtectedResourceMetadata")
            .field("issuer_count", &self.issuers.len())
            .field("scope_count", &self.scopes.len())
            .finish_non_exhaustive()
    }
}

impl ProtectedResourceMetadata {
    pub fn new(authorization_servers: Vec<String>) -> Result<Self, HttpSecurityError> {
        if authorization_servers.is_empty() || authorization_servers.len() > MAX_ISSUERS {
            return Err(HttpSecurityError::InvalidPolicy);
        }
        let mut bytes = 0_usize;
        for (index, issuer) in authorization_servers.iter().enumerate() {
            if !valid_issuer(issuer) || authorization_servers[..index].contains(issuer) {
                return Err(HttpSecurityError::InvalidPolicy);
            }
            bytes = bytes.checked_add(issuer.len()).ok_or(HttpSecurityError::InvalidPolicy)?;
            if bytes > MAX_ISSUER_SET_BYTES { return Err(HttpSecurityError::InvalidPolicy); }
        }
        Ok(Self { issuers: authorization_servers, scopes: Vec::new(), name: None })
    }

    /// Advertised deployment-wide scopes, not scopes granted to a caller.
    /// Each entry is one RFC 6749 scope-token; spaces cannot combine two scopes.
    pub fn with_scopes_supported(mut self, scopes: Vec<String>) -> Result<Self, HttpSecurityError> {
        if scopes.len() > MAX_SCOPES { return Err(HttpSecurityError::InvalidPolicy); }
        let mut bytes = 0_usize;
        for (index, scope) in scopes.iter().enumerate() {
            if scope.is_empty() || scope.len() > MAX_SCOPE_BYTES
                || !scope.bytes().all(|byte| matches!(byte, 0x21 | 0x23..=0x5b | 0x5d..=0x7e))
                || scopes[..index].contains(scope)
            { return Err(HttpSecurityError::InvalidPolicy); }
            bytes = bytes.checked_add(scope.len()).ok_or(HttpSecurityError::InvalidPolicy)?;
            if bytes > MAX_SCOPE_SET_BYTES { return Err(HttpSecurityError::InvalidPolicy); }
        }
        self.scopes = scopes;
        Ok(self)
    }

    pub fn with_resource_name(mut self, name: String) -> Result<Self, HttpSecurityError> {
        if name.is_empty() || name.len() > 1024 || name.chars().any(char::is_control) {
            return Err(HttpSecurityError::InvalidPolicy);
        }
        self.name = Some(name);
        Ok(self)
    }

    fn publish(self, policy: &HttpSecurityPolicy) -> Result<PublishedResourceMetadata, HttpSecurityError> {
        if policy.scheme != "https" { return Err(HttpSecurityError::InvalidPolicy); }
        let origin = policy.public_origin.as_str().trim_end_matches('/');
        let path = policy.endpoint.path();
        let resource = format!("{origin}{path}");
        if resource.len() > MAX_RESOURCE_BYTES { return Err(HttpSecurityError::InvalidPolicy); }
        let canonical = CanonicalHttpUrl::parse(&resource).map_err(|_| HttpSecurityError::InvalidPolicy)?;
        // Dot segments, escaping repairs and other canonicalization must not
        // publish metadata for a path different from the actual configured route.
        if canonical.as_str() != resource { return Err(HttpSecurityError::InvalidPolicy); }
        let route = if path == "/" { WELL_KNOWN.to_owned() } else { format!("{WELL_KNOWN}{path}") };
        let location = format!("{origin}{route}");
        let location_url = CanonicalHttpUrl::parse(&location).map_err(|_| HttpSecurityError::InvalidPolicy)?;
        if location.len() > MAX_RESOURCE_BYTES || location_url.as_str() != location {
            return Err(HttpSecurityError::InvalidPolicy);
        }
        let mut document = json!({
            "resource":resource,
            "authorization_servers":self.issuers,
            "bearer_methods_supported":["header"],
        });
        if !self.scopes.is_empty() { document["scopes_supported"] = json!(self.scopes); }
        if let Some(name) = self.name { document["resource_name"] = json!(name); }
        let mut encoded = BoundedDocument(Vec::new());
        serde_json::to_writer(&mut encoded, &document).map_err(|_| HttpSecurityError::InvalidPolicy)?;
        Ok(PublishedResourceMetadata {
            route, root: path == "/", location: Arc::from(location), body: encoded.0,
        })
    }
}

impl HttpSecurityPolicy {
    /// Publishes metadata at the resource-derived well-known GET route and adds
    /// its fixed URL to ordinary Bearer challenges on admitted MCP responses.
    /// This configures both `handle_secured_async` and `bind_secured_http`.
    /// The public origin must be HTTPS; a native TCP listener still requires
    /// deployment-owned TLS termination that preserves the configured Host.
    ///
    /// Existing explicit resource_metadata parameters or ambiguous/multi-scheme
    /// challenges remain untouched. No caller scope, credential or header is
    /// copied into the metadata. Issuer registration and token verification are
    /// separate explicit server configuration, not inferred from this document.
    pub fn with_resource_metadata(mut self, metadata: ProtectedResourceMetadata) -> Result<Self, HttpSecurityError> {
        self.resource_metadata = Some(Arc::new(metadata.publish(&self)?));
        Ok(self)
    }

    pub fn resource_metadata_url(&self) -> Option<&str> {
        self.resource_metadata.as_ref().map(|metadata| metadata.location.as_ref())
    }

    pub fn resource_metadata_path(&self) -> Option<&str> {
        self.resource_metadata.as_ref().map(|metadata| metadata.route.as_str())
    }

    pub(super) fn is_metadata_path(&self, path: &str) -> bool {
        self.resource_metadata.as_ref().is_some_and(|metadata| metadata.matches(path))
    }
}

pub(super) struct PublishedResourceMetadata {
    route: String,
    root: bool,
    location: Arc<str>,
    body: Vec<u8>,
}

impl PublishedResourceMetadata {
    fn matches(&self, path: &str) -> bool {
        path == self.route
            // The shared client's canonical URL domain retains '/' for a root
            // resource. Admit that one deterministic spelling as well, without
            // enabling prefix matches, redirects or arbitrary resource aliases.
            || (self.root && path.strip_suffix('/') == Some(self.route.as_str()))
    }

    pub(super) fn location(&self) -> Arc<str> { Arc::clone(&self.location) }

    pub(super) fn response(&self, cors: &CorsResponseHeaders) -> HttpResponse {
        let mut response = HttpResponse::ok()
            .with_header("content-type", "application/json")
            .with_header("cache-control", "no-store")
            .with_body(self.body.clone());
        cors.apply_to(&mut response);
        response
    }
}

fn valid_issuer(issuer: &str) -> bool {
    if issuer.len() > MAX_ISSUER_BYTES || !issuer.is_ascii()
        || issuer.bytes().any(|byte| byte <= 32 || byte == 127)
        || issuer.contains(['?', '#', '\\'])
    { return false; }
    let Some(rest) = issuer.strip_prefix("https://") else { return false };
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.is_empty() || authority.contains(['@', '%', ',']) { return false; }
    let Ok(canonical) = CanonicalHttpUrl::parse(issuer) else { return false };
    canonical.as_str() == issuer
        || (canonical.path() == "/" && canonical.as_str().strip_suffix('/') == Some(issuer))
}

struct BoundedDocument(Vec<u8>);
impl Write for BoundedDocument {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_DOCUMENT_BYTES.saturating_sub(self.0.len()) {
            return Err(io::Error::other("protected resource metadata byte limit"));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> { Ok(()) }
}

/// Add only a configured URL to a syntactically unambiguous Bearer challenge.
/// Do not split quoted commas, duplicate parameters or overwrite another
/// authentication component's explicit metadata location. Other schemes and
/// statuses are unchanged; no successful request gains a challenge.
pub(super) fn extend_challenge(response: &mut HttpResponse, location: &str) {
    if !matches!(response.status.0, 401 | 403) { return; }
    let mut challenges = response.headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("www-authenticate"));
    let Some((name, value)) = challenges.next() else {
        if response.status.0 == 401 {
            response.headers.insert("www-authenticate".to_owned(), format!("Bearer resource_metadata=\"{location}\""));
        }
        return;
    };
    if challenges.next().is_some() { return; }
    let value = value.trim_matches([' ', '\t']);
    let Some(has_parameters) = extensible_bearer(value) else { return };
    let separator = if has_parameters { ", " } else { " " };
    let extra = location.len().saturating_add(separator.len() + 20);
    if extra > MAX_CHALLENGE_BYTES.saturating_sub(value.len()) { return; }
    let name = name.clone();
    let value = format!("{value}{separator}resource_metadata=\"{location}\"");
    response.headers.insert(name, value);
}

fn extensible_bearer(value: &str) -> Option<bool> {
    if value.len() > MAX_CHALLENGE_BYTES || !value.is_ascii()
        || value.bytes().any(|byte| byte < 32 && byte != b'\t' || byte == 127)
        || !value.get(..6)?.eq_ignore_ascii_case("Bearer")
    { return None; }
    let mut remaining = &value[6..];
    if remaining.is_empty() { return Some(false); }
    if !remaining.starts_with([' ', '\t']) { return None; }
    remaining = remaining.trim_start_matches([' ', '\t']);
    if remaining.is_empty() { return Some(false); }
    let mut names = Vec::<&str>::new();
    loop {
        if names.len() >= 16 { return None; }
        let end = remaining.bytes().position(|byte| !token_byte(byte)).unwrap_or(remaining.len());
        let name = &remaining[..end];
        if !is_token(name) || name.eq_ignore_ascii_case("resource_metadata")
            || names.iter().any(|old| old.eq_ignore_ascii_case(name))
        { return None; }
        names.push(name);
        remaining = remaining[end..].trim_start_matches([' ', '\t']).strip_prefix('=')?
            .trim_start_matches([' ', '\t']);
        if let Some(quoted) = remaining.strip_prefix('"') {
            let bytes = quoted.as_bytes();
            let mut index = 0;
            loop {
                match bytes.get(index).copied()? {
                    b'"' => { remaining = &quoted[index + 1..]; break; }
                    b'\\' => { index += 1; bytes.get(index)?; }
                    _ => {},
                }
                index += 1;
            }
        } else {
            let end = remaining.bytes().position(|byte| !token_byte(byte)).unwrap_or(remaining.len());
            if end == 0 { return None; }
            remaining = &remaining[end..];
        }
        remaining = remaining.trim_start_matches([' ', '\t']);
        if remaining.is_empty() { return Some(true); }
        remaining = remaining.strip_prefix(',')?.trim_start_matches([' ', '\t']);
    }
}

fn token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte,
        b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-'
        | b'.' | b'^' | b'_' | b'`' | b'|' | b'~')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
    use crate::http_admission::security::{HttpSecurityHead, SecuredModernRequest};
    use fastmcp_transport::http::HttpStatus;

    fn policy(path: &str) -> HttpSecurityPolicy {
        HttpSecurityPolicy::new(
            HttpEndpointConfig::new(path, HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec!["https://app.example".to_owned()],
        ).unwrap()
    }
    fn metadata() -> ProtectedResourceMetadata {
        ProtectedResourceMetadata::new(vec!["https://issuer.example/tenant".to_owned()]).unwrap()
            .with_scopes_supported(vec!["tools:read".to_owned(), "tools:call".to_owned()]).unwrap()
            .with_resource_name("Public MCP deployment".to_owned()).unwrap()
    }
    fn headers() -> Vec<(String, String)> {
        vec![("Host".to_owned(), "service.example".to_owned()),
            ("Origin".to_owned(), "https://app.example".to_owned())]
    }
    fn document(policy: &HttpSecurityPolicy) -> serde_json::Value {
        let SecuredModernRequest::Metadata(response) = policy.admit(
            "GET", policy.resource_metadata_path().unwrap(), &headers(), &[],
        ).unwrap() else { panic!("metadata does not dispatch MCP") };
        assert_eq!(response.status.0, 200);
        assert_eq!(response.headers["content-type"], "application/json");
        assert_eq!(response.headers["cache-control"], "no-store");
        assert_eq!(response.headers["access-control-allow-origin"], "https://app.example");
        serde_json::from_slice(&response.body).unwrap()
    }

    #[test]
    fn metadata_document_and_location_are_bound_to_the_exact_resource() {
        let policy = policy("/tenant/mcp").with_resource_metadata(metadata()).unwrap();
        assert_eq!(policy.resource_metadata_path(), Some("/.well-known/oauth-protected-resource/tenant/mcp"));
        assert_eq!(policy.resource_metadata_url(), Some("https://service.example/.well-known/oauth-protected-resource/tenant/mcp"));
        let result = document(&policy);
        assert_eq!(result["resource"], "https://service.example/tenant/mcp");
        assert_eq!(result["authorization_servers"], json!(["https://issuer.example/tenant"]));
        assert_eq!(result["bearer_methods_supported"], json!(["header"]));
        assert_eq!(result["scopes_supported"], json!(["tools:read", "tools:call"]));
        assert_eq!(result["resource_name"], "Public MCP deployment");
    }

    #[test]
    fn metadata_is_opt_in_and_does_not_turn_get_into_an_mcp_operation() {
        let original = policy("/mcp");
        assert!(matches!(original.admit_head("GET", "/.well-known/oauth-protected-resource/mcp", &headers()),
            Err(HttpSecurityError::EndpointMismatch)));
        let enabled = original.with_resource_metadata(metadata()).unwrap();
        assert!(matches!(enabled.admit_head("GET", "/mcp", &headers()), Err(HttpSecurityError::MethodNotAllowed)));
        let error = enabled.admit_head("POST", enabled.resource_metadata_path().unwrap(), &headers()).unwrap_err();
        assert_eq!(error.response().status.0, 405);
        assert_eq!(error.response().headers["allow"], "GET, OPTIONS");
        for wrong in ["/.well-known/oauth-protected-resource", "/.well-known/oauth-protected-resource/other",
            "/.well-known/oauth-protected-resource/mcp?resource=https://attacker.example"]
        { assert!(matches!(enabled.admit_head("GET", wrong, &headers()), Err(HttpSecurityError::EndpointMismatch))); }
    }

    #[test]
    fn metadata_does_not_use_request_credentials_or_forwarding_to_select_a_document() {
        let policy = policy("/mcp").with_resource_metadata(metadata()).unwrap();
        let expected = document(&policy);
        let mut headers = headers();
        headers.push(("Authorization".to_owned(), "Bearer private-token-canary".to_owned()));
        headers.push(("Forwarded".to_owned(), "host=attacker.example;proto=http".to_owned()));
        let HttpSecurityHead::Metadata(response) = policy.admit_head("GET", policy.resource_metadata_path().unwrap(), &headers).unwrap()
            else { panic!("metadata expected") };
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&response.body).unwrap(), expected);
        assert!(!String::from_utf8_lossy(&response.body).contains("private-token-canary"));
        headers[0].1 = "attacker.example".to_owned();
        assert!(matches!(policy.admit_head("GET", policy.resource_metadata_path().unwrap(), &headers), Err(HttpSecurityError::HostNotAllowed)));
    }

    #[test]
    fn metadata_body_and_origin_refusals_cannot_become_mcp_dispatch() {
        let policy = policy("/mcp").with_resource_metadata(metadata()).unwrap();
        let path = policy.resource_metadata_path().unwrap();
        assert!(matches!(policy.admit("GET", path, &headers(), b"x"), Err(HttpSecurityError::BodyNotAllowed)));
        let mut bad = headers();
        bad[1].1 = "https://attacker.example".to_owned();
        assert!(matches!(policy.admit("GET", path, &bad, &[]), Err(HttpSecurityError::OriginNotAllowed)));
        bad = headers();
        bad.push(("Transfer-Encoding".to_owned(), "chunked".to_owned()));
        assert!(matches!(policy.admit("GET", path, &bad, &[]), Err(HttpSecurityError::BodyNotAllowed)));
    }

    #[test]
    fn metadata_preflight_grants_only_get_and_read_headers() {
        let policy = policy("/mcp").with_resource_metadata(metadata()).unwrap();
        let path = policy.resource_metadata_path().unwrap();
        let mut headers = headers();
        headers.push(("Access-Control-Request-Method".to_owned(), "GET".to_owned()));
        headers.push(("Access-Control-Request-Headers".to_owned(), "Accept".to_owned()));
        let HttpSecurityHead::Preflight(response) = policy.admit_head("OPTIONS", path, &headers).unwrap()
            else { panic!("preflight expected") };
        assert_eq!(response.headers["access-control-allow-methods"], "GET");
        assert_eq!(response.headers["access-control-allow-headers"], "accept");
        headers[2].1 = "POST".to_owned();
        assert!(matches!(policy.admit_head("OPTIONS", path, &headers), Err(HttpSecurityError::InvalidPreflight)));
        headers[2].1 = "GET".to_owned();
        headers[3].1 = "Authorization".to_owned();
        assert!(matches!(policy.admit_head("OPTIONS", path, &headers), Err(HttpSecurityError::HeaderNotAllowed)));
    }

    #[test]
    fn root_resource_keeps_exact_resource_identity_and_bounded_route_spellings() {
        let policy = policy("/").with_resource_metadata(metadata()).unwrap();
        assert_eq!(policy.resource_metadata_path(), Some(WELL_KNOWN));
        assert_eq!(document(&policy)["resource"], "https://service.example/");
        assert!(policy.admit_head("GET", &format!("{WELL_KNOWN}/"), &headers()).is_ok());
        assert!(policy.admit_head("GET", &format!("{WELL_KNOWN}//"), &headers()).is_err());
    }

    #[test]
    fn metadata_configuration_rejects_issuer_repairs_insecure_urls_and_duplicate_authority() {
        for issuer in ["http://issuer.example", "https://user@issuer.example", "https://issuer.example?x=1",
            "https://issuer.example#", "https://ISSUER.example", "https://issuer.example:443",
            "https://issuer.example/a/../b", "https://issuer.example\\path", "https://issuer.example\r\nx:y"]
        { assert!(ProtectedResourceMetadata::new(vec![issuer.to_owned()]).is_err(), "{issuer}"); }
        assert!(ProtectedResourceMetadata::new(Vec::new()).is_err());
        assert!(ProtectedResourceMetadata::new(vec!["https://issuer.example".to_owned(); 2]).is_err());
        assert!(ProtectedResourceMetadata::new(vec!["https://issuer.example".to_owned()]).is_ok());
        assert!(ProtectedResourceMetadata::new(vec!["https://issuer.example/".to_owned()]).is_ok());
    }

    #[test]
    fn scope_and_public_origin_configuration_do_not_widen_authentication() {
        for scope in ["", "read write", "read\"", "read\\", "read\n", "scopé"] {
            assert!(metadata().with_scopes_supported(vec![scope.to_owned()]).is_err());
        }
        assert!(metadata().with_scopes_supported(vec!["read".to_owned(); 2]).is_err());
        let policy = policy("/mcp").with_resource_metadata(metadata().with_scopes_supported(vec![]).unwrap()).unwrap();
        assert!(document(&policy).get("scopes_supported").is_none());
        let insecure = HttpSecurityPolicy::new(self::policy("/mcp").endpoint,
            "http://service.example", vec![]).unwrap();
        assert!(insecure.with_resource_metadata(metadata()).is_err());
        assert!(self::policy("/a/../mcp").with_resource_metadata(metadata()).is_err());
    }

    #[test]
    fn bearer_challenge_retains_errors_and_adds_one_fixed_metadata_parameter() {
        let policy = policy("/mcp").with_resource_metadata(metadata()).unwrap();
        let HttpSecurityHead::Post(cors) = policy.admit_head("POST", "/mcp", &headers()).unwrap()
            else { panic!("POST expected") };
        let original = "Bearer realm=\"service, protected\", error=\"insufficient_scope\", scope=\"tools:call\"";
        let mut response = HttpResponse::new(HttpStatus(403)).with_header("WWW-Authenticate", original)
            .with_body(b"fixed authorization refusal".to_vec());
        cors.apply_to(&mut response);
        assert!(response.headers["www-authenticate"].starts_with(original));
        assert!(response.headers["www-authenticate"].ends_with(&format!(", resource_metadata=\"{}\"", policy.resource_metadata_url().unwrap())));
        let first = response.headers["www-authenticate"].clone();
        cors.apply_to(&mut response);
        assert_eq!(response.headers["www-authenticate"], first);
        assert_eq!(response.status.0, 403);
        assert_eq!(response.body, b"fixed authorization refusal");
    }

    #[test]
    fn complex_or_explicit_challenges_are_not_rewritten_and_success_is_unchanged() {
        let location = "https://service.example/.well-known/oauth-protected-resource/mcp";
        for challenge in ["Basic realm=\"service\"", "Bearer resource_metadata=\"https://configured.example/metadata\"",
            "Bearer RESOURCE_METADATA=\"https://configured.example/metadata\"", "Bearer error=\"x\", Basic realm=\"y\"",
            "Bearer error=\"unterminated", "Bearer error=x, error=y", "Bearer realm=x,", "Bearerevil"]
        {
            let mut response = HttpResponse::new(HttpStatus(401)).with_header("www-authenticate", challenge);
            extend_challenge(&mut response, location);
            assert_eq!(response.headers["www-authenticate"], challenge);
        }
        let mut response = HttpResponse::ok().with_header("www-authenticate", "Bearer");
        extend_challenge(&mut response, location);
        assert_eq!(response.headers["www-authenticate"], "Bearer");
        let mut missing = HttpResponse::new(HttpStatus(401));
        extend_challenge(&mut missing, location);
        assert_eq!(missing.headers["www-authenticate"], format!("Bearer resource_metadata=\"{location}\""));
        let mut forbidden = HttpResponse::new(HttpStatus(403));
        extend_challenge(&mut forbidden, location);
        assert!(!forbidden.headers.contains_key("www-authenticate"));
    }

    #[test]
    fn quoted_challenge_commas_and_escapes_do_not_confuse_parameter_boundaries() {
        assert_eq!(extensible_bearer("Bearer"), Some(false));
        assert_eq!(extensible_bearer("bEaReR realm=\"a, b\\\"c\", error=invalid_token"), Some(true));
        assert_eq!(extensible_bearer("Bearer realm=\"text resource_metadata=not-a-parameter\""), Some(true));
        assert_eq!(extensible_bearer("Bearer realm=\"a\"junk"), None);
        assert_eq!(extensible_bearer("Bearer realm=\"a\\"), None);
        let excessive = (0..17).map(|i| format!("p{i}=x")).collect::<Vec<_>>().join(", ");
        assert_eq!(extensible_bearer(&format!("Bearer {excessive}")), None);
    }

    #[test]
    fn document_and_challenge_bounds_refuse_without_partial_mutation() {
        let mut writer = BoundedDocument(vec![b'x'; MAX_DOCUMENT_BYTES - 1]);
        assert!(writer.write_all(b"ab").is_err());
        assert_eq!(writer.0.len(), MAX_DOCUMENT_BYTES - 1);
        writer.write_all(b"z").unwrap();
        assert_eq!(writer.0.len(), MAX_DOCUMENT_BYTES);
        let mut response = HttpResponse::new(HttpStatus(401)).with_header("www-authenticate", "Bearer");
        response.headers.insert("WWW-Authenticate".to_owned(), "Basic realm=x".to_owned());
        let before = response.headers.clone();
        extend_challenge(&mut response, "https://service.example/metadata");
        assert_eq!(response.headers, before);
        assert!(metadata().with_resource_name("x".repeat(1025)).is_err());
        assert!(metadata().with_scopes_supported(vec!["a".repeat(MAX_SCOPE_BYTES + 1)]).is_err());
        let location = "https://service.example/metadata";
        let suffix = format!(", resource_metadata=\"{location}\"");
        let remaining = MAX_CHALLENGE_BYTES - suffix.len() - "Bearer realm=\"\"".len();
        for (extra, should_extend) in [(0, true), (1, false)] {
            let original = format!("Bearer realm=\"{}\"", "x".repeat(remaining + extra));
            let mut response = HttpResponse::new(HttpStatus(401)).with_header("www-authenticate", &original);
            extend_challenge(&mut response, location);
            if should_extend {
                assert_eq!(response.headers["www-authenticate"].len(), MAX_CHALLENGE_BYTES);
                assert!(response.headers["www-authenticate"].ends_with(&suffix));
            } else {
                assert_eq!(response.headers["www-authenticate"], original);
            }
        }
    }
}
