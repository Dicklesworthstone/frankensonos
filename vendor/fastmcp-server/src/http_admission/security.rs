//! Origin-bound modern HTTP ingress and browser preflight.
//!
//! The public endpoint origin is administrator configuration, never a Host or
//! Forwarded-derived authorization decision. This boundary runs before JSON,
//! authentication and application dispatch. It admits a native POST without
//! Origin, an explicitly allowed browser origin, or a bodyless POST preflight.
//! CORS grants are not authentication and never enable cookie credentials.
//!
//! Embedders must preserve duplicate header fields until admission, enforce
//! transport read limits, and apply the returned CORS receipt to the response
//! head, including error and SSE heads. This module does not bind a listener or
//! infer trusted reverse proxies. A proxy must preserve the configured public
//! Host or pass through its own separately authenticated ingress boundary.

/// Origin-bound entry point to the existing asynchronous HTTP dispatcher.
pub mod endpoint;
/// Explicit resource-bound OAuth discovery and Bearer challenge publication.
pub mod resource_metadata;
/// Revisioned scope implication selected by an authentication provider's host.
pub mod scope_policy;

use std::fmt;
use std::sync::Arc;

use fastmcp_core::CanonicalHttpUrl;
use fastmcp_transport::http::{HttpResponse, HttpStatus};

use super::{AdmittedModernPost, HttpEndpointConfig, ModernPostRejection, admit_modern_post};

const MAX_ORIGINS: usize = 64;
const MAX_ORIGIN_BYTES: usize = 2048;
const MAX_ORIGIN_SET_BYTES: usize = 16 * 1024;
const MAX_REQUEST_HEADERS: usize = 32;
const MAX_HEADER_NAME_BYTES: usize = 128;
const DEFAULT_REQUEST_HEADERS: [&str; 6] = [
    "accept", "authorization", "content-type", "mcp-protocol-version", "mcp-method", "mcp-name",
];
const SINGLETONS: [&str; 11] = [
    "host", "origin", "authorization", "content-type", "content-length", "content-encoding",
    "mcp-protocol-version", "mcp-method", "mcp-name",
    "access-control-request-method", "access-control-request-headers",
];

/// Immutable route, public authority, browser-origin and preflight policy.
/// The configured public origin is admitted alongside the explicit allowlist.
/// Origins use canonical HTTP(S) serialization with no trailing slash. Wildcard,
/// opaque/null, userinfo, path, query, fragment and noncanonical origins fail at
/// construction rather than widening the policy during a request.
#[derive(Clone)]
pub struct HttpSecurityPolicy {
    endpoint: HttpEndpointConfig,
    public_origin: CanonicalHttpUrl,
    scheme: String,
    origins: Vec<String>,
    request_headers: Vec<String>,
    resource_metadata: Option<Arc<resource_metadata::PublishedResourceMetadata>>,
    scope_authorization: Option<scope_policy::request::ScopeRequestPolicy>,
    sse_revalidation: Option<endpoint::revalidation::SseRevalidationPolicy>,
}

impl fmt::Debug for HttpSecurityPolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HttpSecurityPolicy")
            .field("origin_count", &self.origins.len())
            .field("request_header_count", &self.request_headers.len())
            .field("publishes_resource_metadata", &self.resource_metadata.is_some())
            .field("enforces_method_scopes", &self.scope_authorization.is_some())
            .field("revalidates_sse", &self.sse_revalidation.is_some())
            .finish_non_exhaustive()
    }
}

/// Fixed security diagnostics do not reflect request headers, credentials,
/// attacker-selected origins or body bytes.
#[derive(Clone, PartialEq, Eq)]
pub enum HttpSecurityError {
    InvalidPolicy,
    EndpointMismatch,
    MethodNotAllowed,
    MetadataMethodNotAllowed,
    HeaderLimit,
    InvalidHeader,
    DuplicateHeader,
    HostNotAllowed,
    OriginNotAllowed,
    InvalidPreflight,
    HeaderNotAllowed,
    BodyNotAllowed,
    BodyTooLarge,
    ContentLengthMismatch,
    Protocol(ModernPostRejection),
}

impl fmt::Debug for HttpSecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result { fmt::Display::fmt(self, f) }
}

impl fmt::Display for HttpSecurityError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPolicy => "invalid HTTP origin security policy",
            Self::EndpointMismatch => "HTTP endpoint not found",
            Self::MethodNotAllowed | Self::MetadataMethodNotAllowed => "HTTP method not allowed",
            Self::HeaderLimit => "HTTP header bound exceeded",
            Self::InvalidHeader => "invalid HTTP header",
            Self::DuplicateHeader => "duplicate security-sensitive HTTP header",
            Self::HostNotAllowed => "HTTP authority not allowed",
            Self::OriginNotAllowed => "HTTP origin not allowed",
            Self::InvalidPreflight => "invalid HTTP preflight",
            Self::HeaderNotAllowed => "HTTP preflight header not allowed",
            Self::BodyNotAllowed => "HTTP body not allowed on this route",
            Self::BodyTooLarge => "HTTP body bound exceeded",
            Self::ContentLengthMismatch => "HTTP content length does not match body",
            Self::Protocol(_) => "modern HTTP protocol admission failed",
        })
    }
}
impl std::error::Error for HttpSecurityError {}

impl HttpSecurityError {
    /// Fixed empty response for a security refusal. Protocol-specific errors
    /// should instead use the existing modern dispatcher's error mapping.
    pub fn response(&self) -> HttpResponse {
        let status = match self {
            Self::EndpointMismatch => 404,
            Self::MethodNotAllowed | Self::MetadataMethodNotAllowed => 405,
            Self::HeaderLimit => 431,
            Self::BodyTooLarge => 413,
            Self::HostNotAllowed | Self::OriginNotAllowed | Self::HeaderNotAllowed => 403,
            Self::InvalidPolicy => 500,
            _ => 400,
        };
        let mut response = HttpResponse::new(HttpStatus(status))
            .with_header("cache-control", "no-store")
            .with_header("vary", "Origin, Access-Control-Request-Method, Access-Control-Request-Headers");
        if status == 405 {
            let methods = if matches!(self, Self::MetadataMethodNotAllowed) { "GET, OPTIONS" } else { "POST, OPTIONS" };
            response = response.with_header("allow", methods);
        }
        response
    }
}

/// A head-only decision, usable before reading or parsing the request body.
/// Preflight and metadata GET still require the transport to establish an empty
/// body before writing their response. `admit` checks buffered requests.
#[derive(Debug)]
pub enum HttpSecurityHead {
    Post(CorsResponseHeaders),
    Preflight(HttpResponse),
    Metadata(HttpResponse),
}

/// Security admission followed by the existing strict protocol admission.
#[derive(Debug)]
pub enum SecuredModernRequest {
    Post { admitted: AdmittedModernPost, cors: CorsResponseHeaders },
    Preflight(HttpResponse),
    Metadata(HttpResponse),
}

/// Non-forgeable, request-local response grant. Absence of Origin grants no
/// cross-origin access. Apply to successful and failed responses alike, without
/// overwriting authentication challenges, cache policy, content type or body.
/// Explicit resource metadata augments an unambiguous Bearer challenge only.
#[derive(Debug, Clone)]
pub struct CorsResponseHeaders {
    origin: Option<String>,
    metadata_location: Option<Arc<str>>,
}

impl CorsResponseHeaders {
    pub fn allowed_origin(&self) -> Option<&str> { self.origin.as_deref() }

    pub fn apply_to(&self, response: &mut HttpResponse) {
        // Remove stale or application-supplied CORS grants, including differently
        // cased spellings. The admission receipt is the only CORS authority.
        response.headers.retain(|name, _| !name.to_ascii_lowercase().starts_with("access-control-"));
        merge_vary(response, &["Origin"]);
        if let Some(origin) = &self.origin {
            response.headers.insert("access-control-allow-origin".to_owned(), origin.clone());
            response.headers.insert("access-control-expose-headers".to_owned(),
                "WWW-Authenticate, MCP-Protocol-Version, Retry-After".to_owned());
        }
        if let Some(location) = &self.metadata_location {
            resource_metadata::extend_challenge(response, location);
        }
    }
}

impl HttpSecurityPolicy {
    pub fn new(
        endpoint: HttpEndpointConfig,
        public_origin: &str,
        allowed_origins: Vec<String>,
    ) -> Result<Self, HttpSecurityError> {
        if endpoint.path().contains(['?', '#']) || endpoint.path().starts_with("//")
            || allowed_origins.len() > MAX_ORIGINS
        { return Err(HttpSecurityError::InvalidPolicy); }
        let public_url = parse_origin(public_origin, true).ok_or(HttpSecurityError::InvalidPolicy)?;
        let (scheme, _) = public_origin.split_once("://").ok_or(HttpSecurityError::InvalidPolicy)?;
        let mut origins = vec![public_origin.to_owned()];
        let mut bytes = public_origin.len();
        for origin in allowed_origins {
            if parse_origin(&origin, true).is_none() { return Err(HttpSecurityError::InvalidPolicy); }
            if origins.contains(&origin) { continue; }
            bytes = bytes.checked_add(origin.len()).ok_or(HttpSecurityError::InvalidPolicy)?;
            if origins.len() >= MAX_ORIGINS || bytes > MAX_ORIGIN_SET_BYTES {
                return Err(HttpSecurityError::InvalidPolicy);
            }
            origins.push(origin);
        }
        Ok(Self {
            endpoint, public_origin: public_url, scheme: scheme.to_owned(), origins,
            request_headers: DEFAULT_REQUEST_HEADERS.iter().map(|value| (*value).to_owned()).collect(),
            resource_metadata: None,
            scope_authorization: None,
            sse_revalidation: None,
        })
    }

    /// Explicit extra application routing headers. This never grants Host,
    /// Cookie, forwarding, proxy, connection, framing or Access-Control fields.
    /// The final protocol/header-body mirror validator still owns their values.
    pub fn with_request_headers(mut self, extra: Vec<String>) -> Result<Self, HttpSecurityError> {
        if extra.len() > MAX_REQUEST_HEADERS { return Err(HttpSecurityError::InvalidPolicy); }
        for name in extra {
            let name = name.to_ascii_lowercase();
            if name.len() > MAX_HEADER_NAME_BYTES || !is_token(&name) || forbidden_request_header(&name) {
                return Err(HttpSecurityError::InvalidPolicy);
            }
            if !self.request_headers.contains(&name) {
                if self.request_headers.len() >= MAX_REQUEST_HEADERS { return Err(HttpSecurityError::InvalidPolicy); }
                self.request_headers.push(name);
            }
        }
        Ok(self)
    }

    pub fn endpoint(&self) -> &HttpEndpointConfig { &self.endpoint }

    fn admit_route(&self, method: &str, path: &str) -> Result<(), HttpSecurityError> {
        if path == self.endpoint.path() {
            if matches!(method, "POST" | "OPTIONS") { return Ok(()); }
            return Err(HttpSecurityError::MethodNotAllowed);
        }
        if self.is_metadata_path(path) {
            if matches!(method, "GET" | "OPTIONS") { return Ok(()); }
            return Err(HttpSecurityError::MetadataMethodNotAllowed);
        }
        Err(HttpSecurityError::EndpointMismatch)
    }

    pub fn admit_head(
        &self, method: &str, path: &str, headers: &[(String, String)],
    ) -> Result<HttpSecurityHead, HttpSecurityError> {
        self.admit_route(method, path)?;
        let metadata = self.resource_metadata.as_ref().filter(|_| self.is_metadata_path(path));
        let requested_method = if metadata.is_some() { "GET" } else { "POST" };
        self.admit_route_head(method, headers, requested_method, metadata)
    }

    // Only the native HTTPS listener can select an installed issuer route.
    // Its exact method and body contract come from immutable OAuthHttpRoutes;
    // issuer requests still cross the same header, Host and Origin boundary.
    fn admit_oauth_head(
        &self, method: &str, headers: &[(String, String)], bodyless: bool,
    ) -> Result<HttpSecurityHead, HttpSecurityError> {
        let requested_method = if bodyless { "GET" } else { "POST" };
        if method != requested_method && method != "OPTIONS" {
            return Err(if bodyless { HttpSecurityError::MetadataMethodNotAllowed }
                else { HttpSecurityError::MethodNotAllowed });
        }
        let mut admitted = self.admit_route_head(method, headers, requested_method, None)?;
        if let HttpSecurityHead::Post(cors) = &mut admitted {
            // An issuer error must not acquire the co-hosted MCP resource's
            // Bearer challenge metadata.
            cors.metadata_location = None;
        }
        Ok(admitted)
    }

    fn admit_route_head(
        &self, method: &str, headers: &[(String, String)], requested_method: &str,
        metadata: Option<&Arc<resource_metadata::PublishedResourceMetadata>>,
    ) -> Result<HttpSecurityHead, HttpSecurityError> {
        let limits = self.endpoint.limits();
        if headers.len() > limits.max_header_count() { return Err(HttpSecurityError::HeaderLimit); }
        let mut bytes = 0_usize;
        for (name, value) in headers {
            bytes = bytes.checked_add(name.len()).and_then(|n| n.checked_add(value.len()))
                .ok_or(HttpSecurityError::HeaderLimit)?;
            if bytes > limits.max_header_block_bytes() { return Err(HttpSecurityError::HeaderLimit); }
        }
        for (name, value) in headers {
            if !is_token(name) || value.bytes().any(|byte| byte < 32 && byte != b'\t' || byte == 127) {
                return Err(HttpSecurityError::InvalidHeader);
            }
        }
        for name in SINGLETONS {
            if headers.iter().filter(|(key, _)| key.eq_ignore_ascii_case(name)).count() > 1 {
                return Err(HttpSecurityError::DuplicateHeader);
            }
        }
        let host = field(headers, "host").ok_or(HttpSecurityError::HostNotAllowed)?;
        // Canonicalize the authority using the configured scheme, not any
        // peer-supplied forwarding hint. Percent/userinfo/path repairs are denied.
        let origin = format!("{}://{}", self.scheme, host);
        let host_url = parse_origin(&origin, false).ok_or(HttpSecurityError::HostNotAllowed)?;
        if host_url != self.public_origin { return Err(HttpSecurityError::HostNotAllowed); }
        let origin = field(headers, "origin");
        if let Some(origin) = origin
            && (parse_origin(origin, true).is_none() || !self.origins.iter().any(|allowed| allowed == origin))
        { return Err(HttpSecurityError::OriginNotAllowed); }
        if method != "OPTIONS" {
            if headers.iter().any(|(name, _)| name.to_ascii_lowercase().starts_with("access-control-request-")) {
                return Err(HttpSecurityError::InvalidPreflight);
            }
            let cors = CorsResponseHeaders {
                origin: origin.map(str::to_owned),
                metadata_location: self.resource_metadata.as_ref().map(|metadata| metadata.location()),
            };
            return match metadata {
                Some(metadata) => Ok(HttpSecurityHead::Metadata(metadata.response(&cors))),
                None => Ok(HttpSecurityHead::Post(cors)),
            };
        }
        let origin = origin.ok_or(HttpSecurityError::InvalidPreflight)?;
        if field(headers, "access-control-request-method") != Some(requested_method)
            || headers.iter().any(|(name, _)| name.eq_ignore_ascii_case("access-control-request-private-network"))
        { return Err(HttpSecurityError::InvalidPreflight); }
        let requested = self.requested_headers(field(headers, "access-control-request-headers"))?;
        if metadata.is_some() && requested.iter().any(|name| name != "accept") {
            return Err(HttpSecurityError::HeaderNotAllowed);
        }
        let mut response = HttpResponse::new(HttpStatus(204));
        CorsResponseHeaders { origin: Some(origin.to_owned()), metadata_location: None }.apply_to(&mut response);
        response.headers.insert("access-control-allow-methods".to_owned(), requested_method.to_owned());
        if !requested.is_empty() {
            response.headers.insert("access-control-allow-headers".to_owned(), requested.join(", "));
        }
        response.headers.insert("access-control-max-age".to_owned(), "0".to_owned());
        response.headers.insert("cache-control".to_owned(), "no-store".to_owned());
        merge_vary(&mut response, &["Access-Control-Request-Method", "Access-Control-Request-Headers"]);
        Ok(HttpSecurityHead::Preflight(response))
    }

    /// Combines security and strict JSON-RPC admission without authenticating or
    /// dispatching preflight or configured metadata GET. A security refusal takes
    /// precedence over body errors. The original request sidecar stays intact.
    pub fn admit(
        &self, method: &str, path: &str, headers: &[(String, String)], body: &[u8],
    ) -> Result<SecuredModernRequest, HttpSecurityError> {
        let head = self.admit_head(method, path, headers)?;
        self.validate_body(!matches!(&head, HttpSecurityHead::Post(_)), headers, body)?;
        match head {
            HttpSecurityHead::Preflight(response) => Ok(SecuredModernRequest::Preflight(response)),
            HttpSecurityHead::Metadata(response) => Ok(SecuredModernRequest::Metadata(response)),
            HttpSecurityHead::Post(cors) => {
                let admitted = admit_modern_post(&self.endpoint, method, path, headers, body)
                    .map_err(HttpSecurityError::Protocol)?;
                Ok(SecuredModernRequest::Post { admitted, cors })
            }
        }
    }

    // Shared with the real dispatcher adapter so it need not parse JSON twice
    // or replace the existing protocol-specific HTTP/JSON-RPC error mapping.
    fn validate_body(
        &self, bodyless: bool, headers: &[(String, String)], body: &[u8],
    ) -> Result<(), HttpSecurityError> {
        if bodyless && (!body.is_empty() || field(headers, "transfer-encoding").is_some()) {
            return Err(HttpSecurityError::BodyNotAllowed);
        }
        if body.len() > self.endpoint.limits().max_body_bytes() { return Err(HttpSecurityError::BodyTooLarge); }
        if let Some(length) = field(headers, "content-length") {
            if length.is_empty() || !length.bytes().all(|byte| byte.is_ascii_digit())
                || length.parse::<usize>().ok() != Some(body.len())
                || field(headers, "transfer-encoding").is_some()
            { return Err(HttpSecurityError::ContentLengthMismatch); }
        }
        Ok(())
    }

    fn requested_headers(&self, value: Option<&str>) -> Result<Vec<String>, HttpSecurityError> {
        let Some(value) = value else { return Ok(Vec::new()) };
        let mut requested = Vec::new();
        for (count, name) in value.split(',').enumerate() {
            let name = name.trim_matches([' ', '\t']).to_ascii_lowercase();
            if count >= MAX_REQUEST_HEADERS || name.len() > MAX_HEADER_NAME_BYTES || !is_token(&name) {
                return Err(HttpSecurityError::InvalidPreflight);
            }
            if !self.request_headers.contains(&name) { return Err(HttpSecurityError::HeaderNotAllowed); }
            if !requested.contains(&name) { requested.push(name); }
        }
        requested.sort_unstable();
        Ok(requested)
    }
}

fn field<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)).map(|(_, value)| value.as_str())
}

fn parse_origin(value: &str, require_canonical: bool) -> Option<CanonicalHttpUrl> {
    if value.is_empty() || value.len() > MAX_ORIGIN_BYTES || !value.is_ascii()
        || value.bytes().any(|byte| byte <= 32 || byte == 127)
    { return None; }
    let (scheme, authority) = value.split_once("://")?;
    if !matches!(scheme, "http" | "https") || authority.is_empty()
        || authority.contains(['/', '\\', '?', '#', '@', '%', ','])
    { return None; }
    let root = format!("{value}/");
    let canonical = CanonicalHttpUrl::parse(&root).ok()?;
    if require_canonical && canonical.as_str() != root { return None; }
    Some(canonical)
}

fn is_token(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_alphanumeric()
        || matches!(byte, b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-'
            | b'.' | b'^' | b'_' | b'`' | b'|' | b'~'))
}

fn forbidden_request_header(name: &str) -> bool {
    matches!(name, "host" | "cookie" | "cookie2" | "connection" | "content-length"
        | "transfer-encoding" | "te" | "trailer" | "upgrade" | "forwarded"
        | "origin" | "referer" | "accept-encoding" | "accept-charset" | "date" | "via")
        || name.starts_with("access-control-") || name.starts_with("proxy-")
        || name.starts_with("sec-") || name.starts_with("x-forwarded-")
        || name.contains('*')
}

fn merge_vary(response: &mut HttpResponse, required: &[&str]) {
    let mut values = Vec::<String>::new();
    for (name, value) in &response.headers {
        if name.eq_ignore_ascii_case("vary") {
            for value in value.split(',').map(str::trim).filter(|value| !value.is_empty()) {
                if !values.iter().any(|old| old.eq_ignore_ascii_case(value)) { values.push(value.to_owned()); }
            }
        }
    }
    if !values.iter().any(|value| value == "*") {
        for value in required {
            if !values.iter().any(|old| old.eq_ignore_ascii_case(value)) { values.push((*value).to_owned()); }
        }
    }
    response.headers.retain(|name, _| !name.eq_ignore_ascii_case("vary"));
    response.headers.insert("vary".to_owned(), values.join(", "));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_admission::{HttpAdmissionLimits, ResponseRepresentation};
    use fastmcp_protocol::FINAL_PROTOCOL_VERSION;

    fn policy() -> HttpSecurityPolicy {
        HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec!["https://app.example".to_owned()],
        ).unwrap()
    }
    fn headers() -> Vec<(String, String)> {
        [("Host", "service.example"), ("Origin", "https://app.example"),
            ("Content-Type", "application/json"), ("Accept", "application/json"),
            ("MCP-Protocol-Version", FINAL_PROTOCOL_VERSION), ("Mcp-Method", "server/discover")]
            .into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect()
    }
    fn body() -> &'static [u8] {
        br#"{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#
    }
    fn preflight() -> Vec<(String, String)> {
        [("Host", "service.example"), ("Origin", "https://app.example"),
            ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", "Authorization, Content-Type, Mcp-Method, Mcp-Name, MCP-Protocol-Version")]
            .into_iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect()
    }

    #[test]
    fn origin_bound_post_preserves_strict_protocol_admission_and_exact_params() {
        let SecuredModernRequest::Post { admitted, cors } = policy().admit("POST", "/mcp", &headers(), body()).unwrap()
            else { panic!("POST required") };
        assert_eq!(admitted.request().method, "server/discover");
        assert_eq!(admitted.representation(), ResponseRepresentation::Json);
        assert!(admitted.raw_params().unwrap().contains("clientCapabilities"));
        assert_eq!(cors.allowed_origin(), Some("https://app.example"));
        assert!(matches!(policy().admit("POST", "/mcp", &headers(), b"bad JSON"), Err(HttpSecurityError::Protocol(_))));
    }

    #[test]
    fn preflight_needs_no_json_protocol_metadata_or_authentication() {
        let SecuredModernRequest::Preflight(response) = policy().admit("OPTIONS", "/mcp", &preflight(), b"").unwrap()
            else { panic!("preflight required") };
        assert_eq!(response.status.0, 204);
        assert!(response.body.is_empty());
        assert_eq!(response.headers["access-control-allow-methods"], "POST");
        assert_eq!(response.headers["access-control-allow-origin"], "https://app.example");
        assert_eq!(response.headers["access-control-allow-headers"],
            "authorization, content-type, mcp-method, mcp-name, mcp-protocol-version");
        assert!(!response.headers.contains_key("access-control-allow-credentials"));
        assert!(!response.headers.contains_key("www-authenticate"));
    }

    #[test]
    fn forbidden_origin_wins_over_malformed_json_without_mutating_inputs() {
        let mut headers = headers();
        headers[1].1 = "https://attacker.example".to_owned();
        let before = headers.clone();
        assert!(matches!(policy().admit("POST", "/mcp", &headers, b"bad JSON"), Err(HttpSecurityError::OriginNotAllowed)));
        assert_eq!(headers, before);
    }

    #[test]
    fn originless_native_and_exact_same_origin_posts_are_admitted() {
        let mut headers = headers();
        headers.remove(1);
        let SecuredModernRequest::Post { cors, .. } = policy().admit("POST", "/mcp", &headers, body()).unwrap()
            else { panic!("POST required") };
        assert_eq!(cors.allowed_origin(), None);
        headers.push(("Origin".to_owned(), "https://service.example".to_owned()));
        assert!(policy().admit("POST", "/mcp", &headers, body()).is_ok());
    }

    #[test]
    fn untrusted_forwarding_headers_cannot_replace_host_binding() {
        let mut headers = headers();
        headers[0].1 = "attacker.example".to_owned();
        headers.push(("Forwarded".to_owned(), "host=service.example;proto=https".to_owned()));
        headers.push(("X-Forwarded-Host".to_owned(), "service.example".to_owned()));
        assert!(matches!(policy().admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::HostNotAllowed)));
        headers[0].1 = "SERVICE.EXAMPLE:443".to_owned();
        assert!(policy().admit_head("POST", "/mcp", &headers).is_ok());
        headers[0].1 = "service.example:444".to_owned();
        assert!(matches!(policy().admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::HostNotAllowed)));
    }

    #[test]
    fn host_binding_supports_canonical_ipv6_and_nondefault_ports() {
        let policy = HttpSecurityPolicy::new(policy().endpoint, "http://[::1]:8080", vec![]).unwrap();
        let mut headers = headers();
        headers.remove(1);
        headers[0].1 = "[::1]:8080".to_owned();
        assert!(policy.admit_head("POST", "/mcp", &headers).is_ok());
        headers[0].1 = "[::1]:8081".to_owned();
        assert!(matches!(policy.admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::HostNotAllowed)));
    }

    #[test]
    fn origin_serialization_does_not_accept_parser_repairs_or_opaque_values() {
        for origin in ["null", "*", "https://app.example/", "https://app.example?", "https://app.example#",
            "https://user@app.example", "https://APP.example", "https://app.example:443",
            "https://app.example https://other.example", "https://app.example,https://other.example",
            "https://app%2eexample", "https://app.example\\path", " https://app.example"]
        {
            let mut headers = headers();
            headers[1].1 = origin.to_owned();
            assert!(matches!(policy().admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::OriginNotAllowed)), "{origin}");
            assert!(HttpSecurityPolicy::new(policy().endpoint, origin, vec![]).is_err(), "{origin}");
        }
    }

    #[test]
    fn duplicate_security_headers_are_rejected_case_insensitively() {
        for name in ["host", "origin", "content-type", "mcp-method"] {
            let mut headers = headers();
            let value = field(&headers, name).unwrap().to_owned();
            headers.push((name.to_owned(), value));
            assert!(matches!(policy().admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::DuplicateHeader)));
        }
        let mut headers = preflight();
        headers.push(("access-control-request-method".to_owned(), "POST".to_owned()));
        assert!(matches!(policy().admit_head("OPTIONS", "/mcp", &headers), Err(HttpSecurityError::DuplicateHeader)));
    }

    #[test]
    fn malformed_header_syntax_cannot_reach_origin_or_body_parsing() {
        for (name, value) in [("Bad Name", "value"), ("Origin", "https://app.example\r\nx: y"), ("Host", "service.example\0")] {
            let mut headers = headers();
            headers.push((name.to_owned(), value.to_owned()));
            assert!(matches!(policy().admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::InvalidHeader)));
        }
    }

    #[test]
    fn preflight_method_origin_headers_and_private_network_requests_are_explicit() {
        for (name, value, expected) in [
            ("Access-Control-Request-Method", "DELETE", HttpSecurityError::InvalidPreflight),
            ("Access-Control-Request-Method", "post", HttpSecurityError::InvalidPreflight),
            ("Origin", "null", HttpSecurityError::OriginNotAllowed),
            ("Access-Control-Request-Headers", "x-unapproved", HttpSecurityError::HeaderNotAllowed),
            ("Access-Control-Request-Headers", "mcp-method,", HttpSecurityError::InvalidPreflight),
        ] {
            let mut headers = preflight();
            headers.iter_mut().find(|(key, _)| key == name).unwrap().1 = value.to_owned();
            assert_eq!(policy().admit_head("OPTIONS", "/mcp", &headers).err(), Some(expected));
        }
        let mut headers = preflight();
        headers.push(("Access-Control-Request-Private-Network".to_owned(), "true".to_owned()));
        assert!(matches!(policy().admit_head("OPTIONS", "/mcp", &headers), Err(HttpSecurityError::InvalidPreflight)));
    }

    #[test]
    fn preflight_requires_both_its_origin_and_requested_method() {
        for missing in ["Origin", "Access-Control-Request-Method"] {
            let headers: Vec<_> = preflight().into_iter().filter(|(key, _)| key != missing).collect();
            assert!(matches!(policy().admit_head("OPTIONS", "/mcp", &headers), Err(HttpSecurityError::InvalidPreflight)));
        }
    }

    #[test]
    fn preflight_cannot_smuggle_a_body_and_post_cannot_borrow_preflight_authority() {
        assert!(matches!(policy().admit("OPTIONS", "/mcp", &preflight(), body()), Err(HttpSecurityError::BodyNotAllowed)));
        assert!(matches!(policy().admit("POST", "/mcp", &preflight(), body()), Err(HttpSecurityError::InvalidPreflight)));
        let mut headers = headers();
        headers.push(("Content-Length".to_owned(), (body().len() + 1).to_string()));
        assert!(matches!(policy().admit("POST", "/mcp", &headers, body()), Err(HttpSecurityError::ContentLengthMismatch)));
        headers.last_mut().unwrap().1 = body().len().to_string();
        assert!(policy().admit("POST", "/mcp", &headers, body()).is_ok());
    }

    #[test]
    fn explicit_application_headers_are_bounded_without_granting_transport_authority() {
        let policy = policy().with_request_headers(vec!["X-Tenant".to_owned()]).unwrap();
        let mut headers = preflight();
        headers[3].1 = "X-Tenant, mcp-method, MCP-METHOD".to_owned();
        let HttpSecurityHead::Preflight(response) = policy.admit_head("OPTIONS", "/mcp", &headers).unwrap()
            else { panic!("preflight required") };
        assert_eq!(response.headers["access-control-allow-headers"], "mcp-method, x-tenant");
        for name in ["*", "Host", "Cookie", "X-Forwarded-Host", "Proxy-Authorization", "Sec-Fetch-Site", "Bad Name"] {
            assert!(self::policy().with_request_headers(vec![name.to_owned()]).is_err());
        }
        headers[3].1 = vec!["mcp-method"; MAX_REQUEST_HEADERS + 1].join(",");
        assert!(matches!(policy.admit_head("OPTIONS", "/mcp", &headers), Err(HttpSecurityError::InvalidPreflight)));
    }

    #[test]
    fn receipt_preserves_authentication_error_and_vary_without_stale_cors_grants() {
        let HttpSecurityHead::Post(cors) = policy().admit_head("POST", "/mcp", &headers()).unwrap()
            else { panic!("POST required") };
        let mut response = HttpResponse::new(HttpStatus(401))
            .with_header("www-authenticate", "Bearer resource_metadata=\"https://service.example/.well-known/oauth-protected-resource\"")
            .with_header("vary", "Accept, Origin")
            .with_header("access-control-allow-origin", "*")
            .with_header("access-control-allow-credentials", "true")
            .with_body(b"authentication required".to_vec());
        response.headers.insert("Vary".to_owned(), "Accept-Encoding".to_owned());
        let challenge = response.headers["www-authenticate"].clone();
        cors.apply_to(&mut response);
        assert_eq!(response.status.0, 401);
        assert_eq!(response.body, b"authentication required");
        assert_eq!(response.headers["www-authenticate"], challenge);
        assert_eq!(response.headers["access-control-allow-origin"], "https://app.example");
        assert!(!response.headers.contains_key("access-control-allow-credentials"));
        let vary: Vec<_> = response.headers["vary"].split(',').map(str::trim).collect();
        for value in ["Accept", "Origin", "Accept-Encoding"] { assert!(vary.contains(&value)); }
        assert_eq!(response.headers.keys().filter(|name| name.eq_ignore_ascii_case("vary")).count(), 1);
    }

    #[test]
    fn originless_response_has_no_cross_origin_grant_and_preserves_vary_star() {
        let mut headers = headers();
        headers.remove(1);
        let HttpSecurityHead::Post(cors) = policy().admit_head("POST", "/mcp", &headers).unwrap()
            else { panic!("POST required") };
        let mut response = HttpResponse::ok().with_header("vary", "*").with_header("access-control-allow-origin", "*");
        cors.apply_to(&mut response);
        assert!(!response.headers.contains_key("access-control-allow-origin"));
        assert_eq!(response.headers["vary"], "*");
    }

    #[test]
    fn route_and_header_bounds_precede_authority_and_preflight_work() {
        let policy = policy();
        assert!(matches!(policy.admit_head("OPTIONS", "/other", &[]), Err(HttpSecurityError::EndpointMismatch)));
        assert!(matches!(policy.admit_head("GET", "/mcp", &[]), Err(HttpSecurityError::MethodNotAllowed)));
        let mut headers = headers();
        headers.resize(33, ("X".to_owned(), String::new()));
        assert!(matches!(policy.admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::HeaderLimit)));
        let headers = vec![("Host".to_owned(), "x".repeat(8193))];
        assert!(matches!(policy.admit_head("POST", "/mcp", &headers), Err(HttpSecurityError::HeaderLimit)));
    }

    #[test]
    fn security_refusal_is_empty_uncacheable_and_non_oracular() {
        let error = HttpSecurityError::OriginNotAllowed;
        let response = error.response();
        assert_eq!(response.status.0, 403);
        assert!(response.body.is_empty());
        assert_eq!(response.headers["cache-control"], "no-store");
        assert!(!response.headers.contains_key("access-control-allow-origin"));
        assert!(!format!("{error:?} {error}").contains("attacker.example"));
    }
}
