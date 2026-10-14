//! Security admission over the unconsumed native HTTP head, before body decode.
//! The existing H1 codec remains responsible for HTTP framing and body decoding.

use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use asupersync::http::h1::Request;
use asupersync::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use asupersync::stream::StreamExt;
use fastmcp_transport::http::{HttpResponse, HttpStatus};

use super::super::super::{CorsResponseHeaders, HttpSecurityError, HttpSecurityHead, HttpSecurityPolicy};
use crate::{BytesMut, Decoder, Encoder, Framed, HTTP_ACCEPT_CANCEL_POLL, Http1DecodeError, Http1Response, HttpListenerShutdown, NativeHttp1Codec, OAuthHttpRoutes, OAuthNativeH1RouteLimits};

const MAX_REQUEST_LINE_BYTES: usize = 8192;
const MAX_EXPECTATION_MEMBERS: usize = 16;
const CONTINUE_RESPONSE: &[u8] = b"HTTP/1.1 100 Continue\r\n\r\n";

pub(super) enum Ingress {
    Request { request: Request, cors: CorsResponseHeaders },
    Immediate(HttpResponse),
    // Head admission only, never a dispatch or authentication permit.
    Continue,
}

enum AdmittedHead {
    Post { cors: CorsResponseHeaders, send_continue: bool },
    Immediate(HttpResponse),
}

pub(super) struct SecuredCodec {
    inner: NativeHttp1Codec,
    policy: Arc<HttpSecurityPolicy>,
    body_limit: usize,
    oauth_limits: Option<OAuthNativeH1RouteLimits>,
    cors: Option<CorsResponseHeaders>,
    finished: bool,
}

impl SecuredCodec {
    pub(super) fn new(policy: Arc<HttpSecurityPolicy>, body_limit: usize) -> Self {
        let body_limit = body_limit.min(policy.endpoint().limits().max_body_bytes());
        Self { inner: NativeHttp1Codec::new(body_limit, None), policy, body_limit, oauth_limits: None, cors: None, finished: false }
    }

    pub(super) fn with_oauth_routes(mut self, routes: Option<&OAuthHttpRoutes>) -> Self {
        self.inner = NativeHttp1Codec::new(self.body_limit, routes);
        self.oauth_limits = routes.map(OAuthNativeH1RouteLimits::from_routes);
        self
    }

    fn refusal(&mut self, error: HttpSecurityError, source: &mut BytesMut) -> Ingress {
        let mut response = error.response();
        if let Some(cors) = &self.cors { cors.apply_to(&mut response); }
        self.finished = true;
        source.clear();
        Ingress::Immediate(response)
    }

    fn head(&mut self, source: &[u8], end: usize) -> Result<AdmittedHead, HttpSecurityError> {
        let text = std::str::from_utf8(&source[..end]).map_err(|_| HttpSecurityError::InvalidHeader)?;
        let mut lines = text.split("\r\n");
        let line = lines.next().ok_or(HttpSecurityError::InvalidHeader)?;
        if line.len() > MAX_REQUEST_LINE_BYTES { return Err(HttpSecurityError::HeaderLimit); }
        let mut words = line.split(' ');
        let method = words.next().ok_or(HttpSecurityError::InvalidHeader)?;
        let target = words.next().ok_or(HttpSecurityError::InvalidHeader)?;
        let version = words.next().ok_or(HttpSecurityError::InvalidHeader)?;
        if words.next().is_some() || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
            || !target.starts_with('/') || target.starts_with("//") || target.contains('#')
            || target.bytes().any(|byte| byte <= 32 || byte == 127)
        { return Err(HttpSecurityError::InvalidHeader); }
        let path = target.split_once('?').map_or(target, |(path, _)| path);
        if self.policy.is_metadata_path(path) && target != path {
            return Err(HttpSecurityError::EndpointMismatch);
        }
        let limits = self.policy.endpoint().limits();
        let mut headers = Vec::new();
        let mut bytes = 0_usize;
        for line in lines {
            if headers.len() >= limits.max_header_count() { return Err(HttpSecurityError::HeaderLimit); }
            let (name, value) = line.split_once(':').ok_or(HttpSecurityError::InvalidHeader)?;
            // Charge received values before trimming OWS. The policy below
            // sees normalized values and cannot recover discarded padding.
            // Colons and CRLF framing have their own allowance in decode().
            bytes = bytes
                .checked_add(name.len())
                .and_then(|total| total.checked_add(value.len()))
                .ok_or(HttpSecurityError::HeaderLimit)?;
            if bytes > limits.max_header_block_bytes() {
                return Err(HttpSecurityError::HeaderLimit);
            }
            // Do not trim the name: whitespace before ':' and obs-fold are not
            // equivalent to an ordinary field. Keep all duplicate fields intact.
            let value = value.trim_matches([' ', '\t']);
            headers.push((name.to_owned(), value.to_owned()));
        }
        let oauth_limit = self.oauth_limits.as_ref().and_then(|routes| routes.body_limit_for_path(path));
        let head = match oauth_limit {
            Some(limit) => self.policy.admit_oauth_head(method, &headers, limit == 0)?,
            None => self.policy.admit_head(method, path, &headers)?,
        };
        if let HttpSecurityHead::Post(cors) = &head {
            self.cors = Some(cors.clone());
        }
        if let Some(routes) = &self.oauth_limits && oauth_limit.is_some() {
            let query = target.split_once('?').map_or("", |(_, query)| query);
            if path == routes.authorization {
                if query.len() > crate::oauth::MAX_OAUTH_AUTHORIZATION_QUERY_BYTES {
                    return Err(HttpSecurityError::BodyTooLarge);
                }
            } else if !query.is_empty() || target.ends_with('?') {
                return Err(HttpSecurityError::InvalidHeader);
            }
        }
        let length = headers.iter().find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value);
        let encodings = headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("transfer-encoding"))
            .collect::<Vec<_>>();
        if encodings.len() > 1 { return Err(HttpSecurityError::DuplicateHeader); }
        if length.is_some() && !encodings.is_empty() { return Err(HttpSecurityError::ContentLengthMismatch); }
        // Do not encourage a body with framing the native codec cannot accept.
        if encodings.iter().any(|(_, value)| !value.eq_ignore_ascii_case("chunked"))
            || (version == "HTTP/1.0" && !encodings.is_empty())
        { return Err(HttpSecurityError::InvalidHeader); }
        let length = length.map(|value| {
            if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(HttpSecurityError::ContentLengthMismatch);
            }
            value.parse::<usize>().map_err(|_| HttpSecurityError::ContentLengthMismatch)
        }).transpose()?;
        if !matches!(&head, HttpSecurityHead::Post(_)) || oauth_limit == Some(0) {
            // Preflight and public metadata GET are bodyless. Never wait for a
            // claimed body, and reject already-buffered pipelining. The response
            // closes this connection, so later request data cannot dispatch.
            if length.is_some_and(|length| length != 0) || !encodings.is_empty() || source.len() != end + 4 {
                return Err(HttpSecurityError::BodyNotAllowed);
            }
        } else if length.is_some_and(|length| length > oauth_limit.unwrap_or(self.body_limit).min(self.body_limit))
            || (oauth_limit.is_some() && !encodings.is_empty())
        {
            return Err(HttpSecurityError::BodyTooLarge);
        }
        let expects_continue = match continue_expectation(&headers) {
            Ok(expected) => expected,
            Err(()) => {
                let mut response = HttpResponse::new(HttpStatus(417))
                    .with_header("cache-control", "no-store")
                    .with_header("connection", "close");
                if let HttpSecurityHead::Post(cors) = &head { cors.apply_to(&mut response); }
                return Ok(AdmittedHead::Immediate(response));
            }
        };
        match head {
            HttpSecurityHead::Preflight(response) | HttpSecurityHead::Metadata(response) => {
                Ok(AdmittedHead::Immediate(response))
            }
            HttpSecurityHead::Post(cors) => {
                // RFC 9110 10.1.1: never wait for content before acknowledging
                // an admitted HTTP/1.1 expectation. HTTP/1.0 ignores it, and no
                // informational response is needed once content has arrived.
                let send_continue = expects_continue && version == "HTTP/1.1"
                    && (length.is_some_and(|length| length != 0) || !encodings.is_empty())
                    && source.len() == end + 4;
                Ok(AdmittedHead::Post { cors, send_continue })
            }
        }
    }
}

// Expect is a list field. Repeated supported members request ONE interim
// response; an unknown member must not be ignored next to 100-continue.
fn continue_expectation(headers: &[(String, String)]) -> Result<bool, ()> {
    let mut expected = false;
    let mut members = 0_usize;
    for (_, value) in headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("expect")) {
        for member in value.split(',') {
            members += 1;
            if members > MAX_EXPECTATION_MEMBERS { return Err(()); }
            let member = member.trim_matches([' ', '\t']);
            if member.is_empty() { continue; }
            if !member.eq_ignore_ascii_case("100-continue") { return Err(()); }
            expected = true;
        }
    }
    Ok(expected)
}

impl Decoder for SecuredCodec {
    type Item = Ingress;
    type Error = Http1DecodeError;

    fn decode(&mut self, source: &mut BytesMut) -> Result<Option<Ingress>, Http1DecodeError> {
        if self.finished { return Ok(None); }
        if self.cors.is_none() {
            let limits = self.policy.endpoint().limits();
            let maximum = limits.max_header_block_bytes()
                .saturating_add(limits.max_header_count().saturating_mul(4))
                .saturating_add(MAX_REQUEST_LINE_BYTES + 4);
            let end = source.windows(4).position(|window| window == b"\r\n\r\n");
            let Some(end) = end else {
                if source.len() > maximum {
                    return Ok(Some(self.refusal(HttpSecurityError::HeaderLimit, source)));
                }
                // Do not delegate until the complete head has been admitted:
                // the inner decoder is allowed to consume bytes incrementally.
                return Ok(None);
            };
            if end > maximum { return Ok(Some(self.refusal(HttpSecurityError::HeaderLimit, source))); }
            match self.head(source, end) {
                Err(error) => return Ok(Some(self.refusal(error, source))),
                Ok(AdmittedHead::Immediate(response)) => {
                    source.clear();
                    self.finished = true;
                    return Ok(Some(Ingress::Immediate(response)));
                }
                Ok(AdmittedHead::Post { cors, send_continue }) => {
                    self.cors = Some(cors);
                    if send_continue { return Ok(Some(Ingress::Continue)); }
                }
            }
        }
        match self.inner.decode(source) {
            Ok(Some(request)) => {
                self.finished = true;
                let cors = self.cors.take().expect("POST head admission precedes body decoding");
                Ok(Some(Ingress::Request { request, cors }))
            }
            Ok(None) => Ok(None),
            Err(_) => Ok(Some(self.refusal(HttpSecurityError::InvalidHeader, source))),
        }
    }
}

impl Encoder<Http1Response> for SecuredCodec {
    type Error = Http1DecodeError;
    fn encode(&mut self, response: Http1Response, destination: &mut BytesMut) -> Result<(), Self::Error> {
        self.inner.encode(response, destination)
    }
}

/// Read one request while servicing an admitted expectation on the same socket.
/// The connection's outer request timeout spans this entire future, including
/// the interim write: receiving the body never starts a second timeout budget.
/// `writing_continue` stays set on abandonment/failure, so the connection cannot
/// append a final error to a partially written informational response.
pub(super) async fn receive<T: AsyncRead + AsyncWrite + Unpin>(
    cx: &Cx,
    shutdown: &HttpListenerShutdown,
    framed: &mut Framed<T, SecuredCodec>,
    write_timeout: Duration,
    writing_continue: &mut bool,
) -> Option<Result<Ingress, Http1DecodeError>> {
    loop {
        if shutdown.is_requested() || cx.checkpoint().is_err() { return None; }
        let incoming = match asupersync::time::timeout(cx.now(), HTTP_ACCEPT_CANCEL_POLL, framed.next()).await {
            Ok(incoming) => incoming,
            Err(_) => continue,
        };
        if !matches!(&incoming, Some(Ok(Ingress::Continue))) { return incoming; }
        *writing_continue = true;
        let writer = framed.get_mut();
        let write = async {
            let mut writing = std::pin::pin!(async {
                writer.write_all(CONTINUE_RESPONSE).await?;
                writer.flush().await
            });
            loop {
                if shutdown.is_requested() || cx.checkpoint().is_err() { return Err(()); }
                // Retain the partially completed write across cancellation
                // polls. Restarting write_all would duplicate response bytes.
                if let Ok(result) = asupersync::time::timeout(cx.now(), HTTP_ACCEPT_CANCEL_POLL, writing.as_mut()).await {
                    return result.map_err(|_| ());
                }
            }
        };
        if !matches!(asupersync::time::timeout(cx.now(), write_timeout, write).await, Ok(Ok(()))) {
            return None;
        }
        *writing_continue = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};

    fn codec() -> SecuredCodec {
        codec_with_header_limit(2048)
    }

    fn codec_with_header_limit(maximum: usize) -> SecuredCodec {
        let policy = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(16, maximum, 64).unwrap()).unwrap(),
            "https://service.example", vec!["https://app.example".to_owned()],
        ).unwrap();
        SecuredCodec::new(Arc::new(policy), 64)
    }

    fn status(codec: &mut SecuredCodec, wire: &str) -> u16 {
        let mut source = BytesMut::from(wire.as_bytes());
        let Some(Ingress::Immediate(response)) = codec.decode(&mut source).unwrap() else {
            panic!("expected an immediate decision before body read")
        };
        assert!(codec.decode(&mut source).unwrap().is_none());
        response.status.0
    }

    fn oauth_codec() -> SecuredCodec {
        let policy = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 65536).unwrap()).unwrap(),
            "https://service.example", vec!["https://app.example".to_owned()],
        ).unwrap();
        let issuer = Arc::new(crate::oauth::OAuthServer::try_new(crate::oauth::OAuthServerConfig {
            issuer: "https://service.example/".to_owned(), ..Default::default()
        }).unwrap());
        let routes = OAuthHttpRoutes::new(issuer, "https://service.example/oauth").unwrap();
        SecuredCodec::new(Arc::new(policy), 65536).with_oauth_routes(Some(&routes))
    }

    #[test]
    fn oauth_routes_keep_exact_methods_host_origin_and_unmodified_query() {
        let valid = "GET /oauth/authorize?state=one%2Btwo&state=three HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\n\r\n";
        let Some(Ingress::Request { request, cors }) = oauth_codec()
            .decode(&mut BytesMut::from(valid.as_bytes())).unwrap() else {
            panic!("installed issuer GET must reach the existing OAuth parameter admission")
        };
        assert_eq!(request.uri, "/oauth/authorize?state=one%2Btwo&state=three");
        assert_eq!(cors.allowed_origin(), Some("https://app.example"));
        assert!(cors.metadata_location.is_none());
        for (wire, expected) in [
            (valid.replace("Host: service.example", "Host: other.example"), 403),
            (valid.replace("Origin: https://app.example", "Origin: https://attacker.example"), 403),
            (valid.replacen("GET ", "POST ", 1), 405),
            (valid.replace("/oauth/authorize?state=one%2Btwo&state=three", "/oauth/token"), 405),
            (valid.replace("/oauth/authorize?state=one%2Btwo&state=three", "/oauth/authorize/"), 404),
        ] {
            assert_eq!(status(&mut oauth_codec(), &wire), expected);
        }
    }

    #[test]
    fn oauth_form_limit_and_bodyless_authorization_precede_continue_or_body_read() {
        let limit = crate::oauth::MAX_OAUTH_FORM_BODY_BYTES;
        let valid = format!("POST /oauth/token HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\nContent-Length: {limit}\r\nExpect: 100-continue\r\n\r\n");
        assert!(matches!(oauth_codec().decode(&mut BytesMut::from(valid.as_bytes())).unwrap(), Some(Ingress::Continue)));
        let oversized = valid.replace(&format!("Content-Length: {limit}"), &format!("Content-Length: {}", limit + 1));
        let mut source = BytesMut::from(oversized.as_bytes());
        let Some(Ingress::Immediate(response)) = oauth_codec().decode(&mut source).unwrap() else {
            panic!("oversized issuer body must be refused before 100 Continue")
        };
        assert_eq!(response.status.0, 413);
        assert_eq!(response.headers["cache-control"], "no-store");
        assert_eq!(response.headers["access-control-allow-origin"], "https://app.example");
        assert_eq!(status(&mut oauth_codec(), &valid.replace(
            &format!("Content-Length: {limit}"), "Transfer-Encoding: chunked")), 413);
        assert_eq!(status(&mut oauth_codec(),
            "GET /oauth/authorize HTTP/1.1\r\nHost: service.example\r\nContent-Length: 1\r\nExpect: 100-continue\r\n\r\n"), 400);
        assert_eq!(status(&mut oauth_codec(),
            "POST /oauth/token?code=wrong-location HTTP/1.1\r\nHost: service.example\r\nContent-Length: 3\r\nExpect: 100-continue\r\n\r\n"), 400);
        assert_eq!(status(&mut oauth_codec(),
            "OPTIONS /oauth/token HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n\r\n"), 204);
    }

    #[test]
    fn denied_origin_and_host_reject_before_a_claimed_body_arrives() {
        for (host, origin) in [("service.example", "https://attacker.example"),
            ("attacker.example", "https://app.example")]
        {
            let wire = format!("POST /mcp HTTP/1.1\r\nHost: {host}\r\nOrigin: {origin}\r\nContent-Length: 64\r\n\r\n");
            assert_eq!(status(&mut codec(), &wire), 403);
        }
    }

    #[test]
    fn exact_method_and_duplicate_authority_are_checked_before_normalization() {
        assert_eq!(status(&mut codec(), "post /mcp HTTP/1.1\r\nHost: service.example\r\n\r\n"), 405);
        assert_eq!(status(&mut codec(), "POST /mcp HTTP/1.1\r\nHost: service.example\r\nhOsT: service.example\r\n\r\n"), 400);
        assert_eq!(status(&mut codec(), "POST /mcp HTTP/1.1\r\nHost : service.example\r\n\r\n"), 400);
    }

    #[test]
    fn oversized_and_ambiguous_framing_reject_without_waiting_for_body() {
        for framing in ["Content-Length: 65", "Content-Length: 2\r\nTransfer-Encoding: chunked",
            "Content-Length: +2", "Content-Length: 2, 2", "Transfer-Encoding: chunked\r\nTransfer-Encoding: chunked"]
        {
            let wire = format!("POST /mcp HTTP/1.1\r\nHost: service.example\r\n{framing}\r\n\r\n");
            let expected = if framing == "Content-Length: 65" { 413 } else { 400 };
            assert_eq!(status(&mut codec(), &wire), expected);
        }
    }

    #[test]
    fn valid_preflight_is_immediate_but_body_claims_are_not() {
        let prefix = "OPTIONS /mcp HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\n";
        assert_eq!(status(&mut codec(), &format!("{prefix}\r\n")), 204);
        assert_eq!(status(&mut codec(), &format!("{prefix}Content-Length: 0\r\n\r\n")), 204);
        assert_eq!(status(&mut codec(), &format!("{prefix}Content-Length: 1\r\n\r\n")), 400);
        assert_eq!(status(&mut codec(), &format!("{prefix}\r\nx")), 400);
    }

    #[test]
    fn partial_head_cannot_be_consumed_before_security_admission() {
        let mut codec = codec();
        let mut source = BytesMut::from(&b"POST /mcp HTTP/1.1\r\nHost: service.example\r\nContent-Length: 2\r\n"[..]);
        let original = source.to_vec();
        assert!(codec.decode(&mut source).unwrap().is_none());
        assert_eq!(&source[..], &original);
        source.extend_from_slice(b"\r\n{}");
        let Some(Ingress::Request { request, cors }) = codec.decode(&mut source).unwrap() else {
            panic!("native H1 parser must finish the admitted request")
        };
        assert_eq!(request.body, b"{}");
        assert_eq!(cors.allowed_origin(), None);
    }

    #[test]
    fn query_and_header_cardinality_survive_native_body_decode() {
        let mut codec = codec();
        let mut source = BytesMut::from(&b"POST /mcp?access_token=x HTTP/1.1\r\nHost: service.example\r\nAccept: application/json\r\nAccept: text/event-stream\r\nContent-Length: 2\r\n\r\n{}"[..]);
        let Some(Ingress::Request { request, .. }) = codec.decode(&mut source).unwrap() else {
            panic!("HTTP framing should not interpret the later credential policy")
        };
        assert_eq!(request.uri, "/mcp?access_token=x");
        assert_eq!(request.headers.iter().filter(|(name, _)| name.eq_ignore_ascii_case("accept")).count(), 2);
    }

    #[test]
    fn unfinished_headers_and_extra_pipeline_bytes_are_bounded() {
        let mut codec = codec();
        let mut source = BytesMut::from("X".repeat(12_000).as_bytes());
        assert!(matches!(codec.decode(&mut source).unwrap(), Some(Ingress::Immediate(_))));
        let mut codec = self::codec();
        let mut source = BytesMut::from(&b"POST /mcp HTTP/1.1\r\nHost: service.example\r\nContent-Length: 2\r\n\r\n{}NEXT"[..]);
        assert!(matches!(codec.decode(&mut source).unwrap(), Some(Ingress::Request { .. })));
        assert!(!source.is_empty(), "connection owner must reject buffered pipelining before dispatch");
    }

    fn metadata_codec() -> SecuredCodec {
        use crate::http_admission::security::resource_metadata::ProtectedResourceMetadata;
        let policy = (*codec().policy).clone().with_resource_metadata(ProtectedResourceMetadata::new(
            vec!["https://issuer.example".to_owned()],
        ).unwrap()).unwrap();
        SecuredCodec::new(Arc::new(policy), 64)
    }
    const METADATA_HEAD: &str = "GET /.well-known/oauth-protected-resource/mcp HTTP/1.1\r\nHost: service.example\r\n";

    #[test]
    fn native_metadata_waits_for_admitted_headers_but_never_for_authentication() {
        let mut codec = metadata_codec();
        let mut source = BytesMut::from(METADATA_HEAD.as_bytes());
        let before = source.to_vec();
        assert!(codec.decode(&mut source).unwrap().is_none());
        assert_eq!(source.as_ref(), before.as_slice());
        source.extend_from_slice(b"\r\n");
        let Some(Ingress::Immediate(response)) = codec.decode(&mut source).unwrap()
            else { panic!("metadata must complete before a request is dispatched") };
        assert_eq!(response.status.0, 200);
        let result: serde_json::Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(result["resource"], "https://service.example/mcp");
        assert_eq!(result["authorization_servers"], serde_json::json!(["https://issuer.example"]));
        assert!(codec.decode(&mut source).unwrap().is_none());
    }

    #[test]
    fn native_metadata_rejects_body_claims_pipelining_and_query_aliases() {
        for tail in ["Content-Length: 1\r\n\r\n", "Transfer-Encoding: chunked\r\n\r\n", "\r\nx"] {
            assert_eq!(status(&mut metadata_codec(), &format!("{METADATA_HEAD}{tail}")), 400);
        }
        assert_eq!(status(&mut metadata_codec(), &format!("{METADATA_HEAD}Content-Length: 0\r\n\r\n")), 200);
        let query = METADATA_HEAD.replace("/mcp HTTP", "/mcp?resource=other HTTP");
        assert_eq!(status(&mut metadata_codec(), &format!("{query}\r\n")), 404);
        assert_eq!(status(&mut metadata_codec(), &format!("{}\r\n", METADATA_HEAD.replacen("GET", "get", 1))), 405);
        assert_eq!(status(&mut codec(), &format!("{METADATA_HEAD}\r\n")), 404);
    }

    #[test]
    fn native_metadata_keeps_origin_and_duplicate_host_rejections_before_dispatch() {
        assert_eq!(status(&mut metadata_codec(), &format!("{METADATA_HEAD}Origin: https://attacker.example\r\n\r\n")), 403);
        assert_eq!(status(&mut metadata_codec(), &format!("{METADATA_HEAD}hOsT: service.example\r\n\r\n")), 400);
        let wire = METADATA_HEAD.replace("Host: service.example", "Host: attacker.example\r\nForwarded: host=service.example;proto=https");
        assert_eq!(status(&mut metadata_codec(), &format!("{wire}\r\n")), 403);
    }

    #[test]
    fn native_mcp_receipt_adds_metadata_to_authentication_errors_only() {
        let mut codec = metadata_codec();
        let mut source = BytesMut::from(&b"POST /mcp HTTP/1.1\r\nHost: service.example\r\nContent-Length: 2\r\n\r\n{}"[..]);
        let Some(Ingress::Request { cors, .. }) = codec.decode(&mut source).unwrap()
            else { panic!("MCP POST still requires downstream protocol/auth admission") };
        let mut response = HttpResponse::new(fastmcp_transport::http::HttpStatus(401)).with_header("www-authenticate", "Bearer");
        cors.apply_to(&mut response);
        assert_eq!(response.headers["www-authenticate"], "Bearer resource_metadata=\"https://service.example/.well-known/oauth-protected-resource/mcp\"");
        let mut success = HttpResponse::ok();
        cors.apply_to(&mut success);
        assert!(!success.headers.contains_key("www-authenticate"));
    }

    const EXPECT_HEAD: &str = "POST /mcp HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\nExpect: 100-continue\r\nContent-Length: 2\r\n\r\n";

    #[test]
    fn expect_continue_admits_the_head_once_without_consuming_or_dispatching_it() {
        for framing in ["Content-Length: 2", "Transfer-Encoding: chunked"] {
            let head = EXPECT_HEAD.replace("Content-Length: 2", framing);
            let mut codec = codec();
            let mut source = BytesMut::from(head.as_bytes());
            assert!(matches!(codec.decode(&mut source).unwrap(), Some(Ingress::Continue)));
            assert_eq!(source.as_ref(), head.as_bytes());
            assert!(codec.decode(&mut source).unwrap().is_none(), "no second acknowledgement while waiting for body");
            let body: &[u8] = if framing == "Content-Length: 2" { b"{}" } else { b"2\r\n{}\r\n0\r\n\r\n" };
            source.extend_from_slice(body);
            let Some(Ingress::Request { request, cors }) = codec.decode(&mut source).unwrap()
                else { panic!("the same admitted head must decode its later body") };
            assert_eq!(request.body, b"{}");
            assert_eq!(cors.allowed_origin(), Some("https://app.example"));
            assert!(codec.decode(&mut source).unwrap().is_none());
        }
    }

    #[test]
    fn expect_continue_never_precedes_security_or_framing_refusal() {
        for (from, to, expected) in [
            ("Host: service.example", "Host: attacker.example", 403),
            ("https://app.example", "https://attacker.example", 403),
            ("Content-Length: 2", "Content-Length: 65", 413),
            ("Content-Length: 2", "Content-Length: 2\r\nTransfer-Encoding: chunked", 400),
            ("Content-Length: 2", "Transfer-Encoding: gzip", 400),
            ("Expect: 100-continue", "Expect: 100-continue, unsupported", 417),
            ("Expect: 100-continue", "Expect: 100-continue;extension=1", 417),
        ] {
            assert_eq!(status(&mut codec(), &EXPECT_HEAD.replace(from, to)), expected);
        }
    }

    #[test]
    fn expect_continue_lists_are_case_insensitive_bounded_and_acknowledged_once() {
        let head = EXPECT_HEAD.replace("Expect: 100-continue", "eXpEcT: , 100-ConTinue,\r\nExpect: 100-continue");
        let mut codec = codec();
        let mut source = BytesMut::from(head.as_bytes());
        assert!(matches!(codec.decode(&mut source).unwrap(), Some(Ingress::Continue)));
        assert!(codec.decode(&mut source).unwrap().is_none());
        let excess = vec!["100-continue"; MAX_EXPECTATION_MEMBERS + 1].join(",");
        assert_eq!(status(&mut self::codec(), &EXPECT_HEAD.replace("100-continue", &excess)), 417);
    }

    #[test]
    fn expect_continue_is_not_emitted_for_http10_buffered_content_or_bodyless_routes() {
        let mut old = codec();
        let mut source = BytesMut::from(EXPECT_HEAD.replace("HTTP/1.1", "HTTP/1.0").as_bytes());
        assert!(old.decode(&mut source).unwrap().is_none());
        source.extend_from_slice(b"{}");
        assert!(matches!(old.decode(&mut source).unwrap(), Some(Ingress::Request { .. })));
        let mut source = BytesMut::from(format!("{EXPECT_HEAD}{{}}").as_bytes());
        assert!(matches!(codec().decode(&mut source).unwrap(), Some(Ingress::Request { .. })));
        let mut source = BytesMut::from(EXPECT_HEAD.replace("Content-Length: 2", "Content-Length: 0").as_bytes());
        assert!(matches!(codec().decode(&mut source).unwrap(), Some(Ingress::Request { .. })));
        assert_eq!(status(&mut metadata_codec(), &format!("{METADATA_HEAD}Expect: 100-continue\r\n\r\n")), 200);
        assert_eq!(status(&mut codec(), "OPTIONS /mcp HTTP/1.1\r\nHost: service.example\r\nOrigin: https://app.example\r\nAccess-Control-Request-Method: POST\r\nExpect: 100-continue\r\n\r\n"), 204);
    }

    #[test]
    fn unsupported_expectation_is_an_empty_uncacheable_cors_bound_final_response() {
        let mut source = BytesMut::from(EXPECT_HEAD.replace("100-continue", "private-expectation-canary").as_bytes());
        let mut codec = codec();
        let Some(Ingress::Immediate(response)) = codec.decode(&mut source).unwrap()
            else { panic!("unsupported expectations must not wait for a body") };
        assert_eq!(response.status.0, 417);
        assert!(response.body.is_empty());
        assert_eq!(response.headers["cache-control"], "no-store");
        assert_eq!(response.headers["connection"], "close");
        assert_eq!(response.headers["access-control-allow-origin"], "https://app.example");
        assert!(!format!("{:?}", response.headers).contains("private-expectation-canary"));
        assert!(source.is_empty());
        assert!(codec.decode(&mut source).unwrap().is_none());
    }

    // A duplex peer that withholds its body until the COMPLETE interim response
    // is flushed. Short writes and Pending polls exercise the production reader,
    // not a second implementation of its state machine.
    struct ExpectPeer {
        head: std::io::Cursor<Vec<u8>>,
        body: std::io::Cursor<Vec<u8>>,
        written: Vec<u8>,
        flushed: bool,
        pending_write: bool,
        fail_write: bool,
        body_reads: usize,
    }

    impl AsyncRead for ExpectPeer {
        fn poll_read(
            self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, buf: &mut asupersync::io::ReadBuf<'_>,
        ) -> std::task::Poll<std::io::Result<()>> {
            use std::io::Read;
            let this = self.get_mut();
            let mut bytes = [0_u8; 512];
            let limit = bytes.len().min(buf.remaining());
            let count = if this.head.position() < this.head.get_ref().len() as u64 {
                this.head.read(&mut bytes[..limit])?
            } else {
                if !this.flushed { return std::task::Poll::Pending; }
                this.body_reads += 1;
                this.body.read(&mut bytes[..limit])?
            };
            buf.put_slice(&bytes[..count]);
            std::task::Poll::Ready(Ok(()))
        }
    }

    impl AsyncWrite for ExpectPeer {
        fn poll_write(
            self: std::pin::Pin<&mut Self>, task: &mut std::task::Context<'_>, bytes: &[u8],
        ) -> std::task::Poll<std::io::Result<usize>> {
            let this = self.get_mut();
            if this.fail_write && !this.written.is_empty() {
                return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
            }
            if this.pending_write {
                this.pending_write = false;
                task.waker().wake_by_ref();
                return std::task::Poll::Pending;
            }
            this.pending_write = true;
            let count = bytes.len().min(3);
            this.written.extend_from_slice(&bytes[..count]);
            std::task::Poll::Ready(Ok(count))
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            let this = self.get_mut();
            assert_eq!(this.written, CONTINUE_RESPONSE);
            this.flushed = true;
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn expect_continue_reader_flushes_once_and_never_reads_body_after_failed_interim_write() {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(async {
                let cx = Cx::current().unwrap();
                for fail_write in [false, true] {
                    let peer = ExpectPeer {
                        head: std::io::Cursor::new(EXPECT_HEAD.as_bytes().to_vec()),
                        body: std::io::Cursor::new(b"{}".to_vec()), written: Vec::new(),
                        flushed: false, pending_write: false, fail_write, body_reads: 0,
                    };
                    let mut framed = Framed::new(peer, codec());
                    let shutdown = HttpListenerShutdown::new(&cx);
                    let mut writing_continue = false;
                    let result = asupersync::time::timeout(cx.now(), Duration::from_secs(1), receive(
                        &cx, &shutdown, &mut framed, Duration::from_secs(1), &mut writing_continue,
                    )).await.expect("a peer waiting for Continue must not deadlock");
                    if fail_write {
                        assert!(result.is_none());
                        assert!(writing_continue, "partial response must prevent a later final error write");
                        assert_eq!(framed.get_ref().written, &CONTINUE_RESPONSE[..3]);
                        assert_eq!(framed.get_ref().body_reads, 0);
                    } else {
                        let Some(Ok(Ingress::Request { request, .. })) = result
                            else { panic!("flushed Continue must release the peer's request body") };
                        assert_eq!(request.body, b"{}");
                        assert!(!writing_continue);
                        assert_eq!(framed.get_ref().written, CONTINUE_RESPONSE);
                        assert_eq!(framed.get_ref().body_reads, 1);
                    }
                    assert!(cx.checkpoint().is_ok());
                }
            });
    }

    fn padded_expect_head(whitespace: &str, extra: usize) -> String {
        // The four existing fields charge 81 bytes including their separator
        // spaces; X-Pad adds five name bytes. Forty-two padding bytes reach
        // exactly 128. Colons, CRLF and the request line are not field values.
        assert_eq!(whitespace.len(), 1);
        format!(
            "{}X-Pad:{}\r\n\r\n",
            EXPECT_HEAD.strip_suffix("\r\n").unwrap(),
            whitespace.repeat(42 + extra)
        )
    }

    #[test]
    fn header_padding_budget_is_exact_and_independent_of_chunk_boundaries() {
        for whitespace in [" ", "\t"] {
            for extra in [0, 1] {
                let head = padded_expect_head(whitespace, extra);
                for split in 0..head.len() {
                    let mut codec = codec_with_header_limit(128);
                    let mut source = BytesMut::from(&head.as_bytes()[..split]);
                    assert!(codec.decode(&mut source).unwrap().is_none());
                    assert_eq!(source.as_ref(), &head.as_bytes()[..split]);
                    source.extend_from_slice(&head.as_bytes()[split..]);
                    let decision = codec.decode(&mut source).unwrap();
                    if extra == 0 {
                        assert!(matches!(decision, Some(Ingress::Continue)));
                        assert_eq!(source.as_ref(), head.as_bytes());
                        source.extend_from_slice(b"{}");
                        let Some(Ingress::Request { request, cors }) =
                            codec.decode(&mut source).unwrap()
                        else {
                            panic!("at-limit padding must preserve ordinary body decoding");
                        };
                        assert_eq!(request.body, b"{}");
                        assert_eq!(cors.allowed_origin(), Some("https://app.example"));
                    } else {
                        let Some(Ingress::Immediate(response)) = decision else {
                            panic!("one extra padding byte must refuse before Continue or dispatch");
                        };
                        assert_eq!(response.status.0, 431);
                        assert!(response.body.is_empty());
                        assert_eq!(response.headers["cache-control"], "no-store");
                        assert!(source.is_empty());
                        assert!(codec.cors.is_none());
                        source.extend_from_slice(EXPECT_HEAD.as_bytes());
                        source.extend_from_slice(b"{}");
                        assert!(codec.decode(&mut source).unwrap().is_none());
                    }
                }
            }
        }
    }

    #[test]
    fn bodyless_metadata_cannot_bypass_the_header_padding_budget() {
        for extra in [0, 1] {
            // Host plus its untrimmed value charge 20 bytes; X-Pad charges
            // another five. Its remaining padding reaches the 2048-byte cap.
            let head = format!(
                "{METADATA_HEAD}X-Pad:{}\r\n\r\n",
                "\t".repeat(2048 - 25 + extra)
            );
            assert_eq!(
                status(&mut metadata_codec(), &head),
                if extra == 0 { 200 } else { 431 }
            );
        }
    }

    #[test]
    fn oversized_padding_never_writes_continue_or_reads_the_request_body() {
        let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build()
            .unwrap();
        runtime.block_on(async {
            let cx = Cx::current().unwrap();
            for extra in [0, 1] {
                let peer = ExpectPeer {
                    head: std::io::Cursor::new(padded_expect_head(" ", extra).into_bytes()),
                    body: std::io::Cursor::new(b"{}".to_vec()),
                    written: Vec::new(),
                    flushed: false,
                    pending_write: false,
                    fail_write: false,
                    body_reads: 0,
                };
                let mut framed = Framed::new(peer, codec_with_header_limit(128));
                let shutdown = HttpListenerShutdown::new(&cx);
                let mut writing_continue = false;
                let result = asupersync::time::timeout(
                    cx.now(),
                    Duration::from_secs(1),
                    receive(
                        &cx,
                        &shutdown,
                        &mut framed,
                        Duration::from_secs(1),
                        &mut writing_continue,
                    ),
                )
                .await
                .expect("header admission must settle without waiting for an unacknowledged body");
                if extra == 0 {
                    let Some(Ok(Ingress::Request { request, .. })) = result else {
                        panic!("at-limit head must still acknowledge and receive its body");
                    };
                    assert_eq!(request.body, b"{}");
                    assert_eq!(framed.get_ref().written, CONTINUE_RESPONSE);
                    assert_eq!(framed.get_ref().body_reads, 1);
                } else {
                    let Some(Ok(Ingress::Immediate(response))) = result else {
                        panic!("over-limit head must produce only a final refusal");
                    };
                    assert_eq!(response.status.0, 431);
                    assert!(framed.get_ref().written.is_empty());
                    assert!(!framed.get_ref().flushed);
                    assert_eq!(framed.get_ref().body_reads, 0);
                }
                assert!(!writing_continue);
                assert!(cx.checkpoint().is_ok());
            }
        });
        assert!(runtime.shutdown_timeout(Duration::from_secs(1)));
    }
}
