//! GH332: optional empty HTTP fields must not block MCP initialization.
//!
//! Exercise the public request-admission and raw HTTP transport paths, not a
//! reimplementation of their validators. Agent Mail's raw-wire probe covers
//! the separately built application's initialization response.

use std::io::Cursor;

use fastmcp_transport::http::{
    HttpError, HttpMethod, HttpRequest, HttpRequestHandler, HttpResponse, HttpTransport,
};

const INITIALIZE: &str = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"gh332","version":"1"}}}"#;

fn initialize_request() -> HttpRequest {
    HttpRequest::new(HttpMethod::Post, "/mcp/v1")
        .with_header("content-type", "application/json")
        .with_header("accept", "application/json, text/event-stream")
        .with_body(INITIALIZE.as_bytes().to_vec())
}

fn wire_request(name: &str, value: &str) -> Vec<u8> {
    let mut wire = String::from("POST /mcp/v1 HTTP/1.1\r\n");
    for (default_name, default_value) in [
        ("Host", "localhost".to_string()),
        ("Content-Type", "application/json".to_string()),
        ("Content-Length", INITIALIZE.len().to_string()),
    ] {
        if !name.eq_ignore_ascii_case(default_name)
            && !(name.eq_ignore_ascii_case("transfer-encoding") && default_name == "Content-Length")
        {
            wire.push_str(&format!("{default_name}: {default_value}\r\n"));
        }
    }
    wire.push_str(&format!("{name}:{value}\r\n\r\n{INITIALIZE}"));
    wire.into_bytes()
}

#[test]
fn gh332_initialize_preserves_empty_extension_headers_in_public_requests() {
    let handler = HttpRequestHandler::new();
    let baseline = handler.parse_request(&initialize_request()).unwrap();
    for name in ["Tailscale-User-Profile-Pic", "X-Optional-Metadata"] {
        for value in ["", " ", "\t", " \t ", "https://example.test/avatar.png"] {
            let mut request = initialize_request();
            // Framework adapters can bypass the builder's normalization.
            request.headers.insert(name.to_string(), value.to_string());
            let parsed = handler
                .parse_request(&request)
                .expect("optional fields must not block initialize");
            assert_eq!(parsed.method, "initialize");
            assert_eq!(parsed.id, baseline.id);
            assert_eq!(parsed.params, baseline.params);
            assert_eq!(request.header(name), Some(value));
        }
    }
}

#[test]
fn gh332_initialize_accepts_optional_empty_fields_on_the_wire() {
    for name in [
        "Tailscale-User-Profile-Pic",
        "tAiLsCaLe-UsEr-PrOfIlE-pIc",
        "X-Optional-Metadata",
    ] {
        for value in ["", " ", "\t", " \t "] {
            let mut transport =
                HttpTransport::new(Cursor::new(wire_request(name, value)), Vec::new());
            let request = transport.read_request().expect("valid HTTP request");
            assert_eq!(request.header(name), Some(""));
            assert_eq!(request.body, INITIALIZE.as_bytes());
            let parsed = HttpRequestHandler::new()
                .parse_request(&request)
                .expect("initialize admission");
            assert_eq!(parsed.method, "initialize");
        }
    }
}

#[test]
fn gh332_required_fields_remain_nonempty_on_both_ingress_paths() {
    for name in [
        "Authorization",
        "Proxy-Authorization",
        "Content-Type",
        "Content-Length",
        "Content-Encoding",
        "Transfer-Encoding",
        "Host",
        "Origin",
        "MCP-Protocol-Version",
        "MCP-Session-Id",
        "MCP-Method",
        "MCP-Name",
    ] {
        for value in ["", " ", "\t", " \t "] {
            let request = initialize_request().with_header(name, value);
            assert!(
                matches!(
                    HttpRequestHandler::new().parse_request(&request),
                    Err(HttpError::InvalidHeader(_))
                ),
                "public request admitted blank {name}",
            );
            let mut transport =
                HttpTransport::new(Cursor::new(wire_request(name, value)), Vec::new());
            assert!(
                matches!(transport.read_request(), Err(HttpError::InvalidHeader(_))),
                "wire request admitted blank {name}",
            );
            assert!(matches!(transport.read_request(), Err(HttpError::Closed)));
        }
    }
}

#[test]
fn gh332_empty_extensions_do_not_disable_name_or_control_validation() {
    for (name, value) in [
        ("Bad Header", "value"),
        ("", "value"),
        ("X-Invalid", "a\0b"),
        ("X-Invalid", "a\rb"),
        ("X-Invalid", "a\nb"),
        ("X-Invalid", "a\x01b"),
        ("X-Invalid", "a\x7fb"),
    ] {
        let request = initialize_request()
            .with_header("Tailscale-User-Profile-Pic", "")
            .with_header(name, value);
        assert!(matches!(
            HttpRequestHandler::new().parse_request(&request),
            Err(HttpError::InvalidHeader(_))
        ));
        let mut transport = HttpTransport::new(Cursor::new(wire_request(name, value)), Vec::new());
        assert!(matches!(
            transport.read_request(),
            Err(HttpError::InvalidHeader(_))
        ));
    }
}

#[test]
fn gh332_case_variant_duplicates_remain_rejected() {
    let mut request = initialize_request().with_header("x-optional-metadata", "");
    request
        .headers
        .insert("X-Optional-Metadata".into(), "value".into());
    assert!(matches!(
        HttpRequestHandler::new().parse_request(&request),
        Err(HttpError::InvalidHeader(_))
    ));
    let wire = wire_request("X-Optional-Metadata", "\r\nx-optional-metadata:value");
    let mut transport = HttpTransport::new(Cursor::new(wire), Vec::new());
    assert!(matches!(
        transport.read_request(),
        Err(HttpError::InvalidHeader(_))
    ));
}

#[test]
fn gh332_optional_empty_response_fields_are_serialized() {
    let mut wire = Vec::new();
    {
        let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), &mut wire);
        let response = HttpResponse::ok().with_header("x-optional-metadata", "");
        transport
            .write_response(&response)
            .expect("valid empty extension field");
    }
    let text = String::from_utf8(wire).expect("ASCII response");
    assert!(text.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(text.contains("\r\nx-optional-metadata: \r\n"));
    assert!(text.contains("\r\ncontent-length: 0\r\n"));
    assert!(text.ends_with("\r\n\r\n"));
}

#[test]
fn gh332_invalid_response_fields_fail_before_any_bytes_are_written() {
    for (name, value) in [
        ("Content-Type", ""),
        ("Content-Type", " \t "),
        ("Content-Length", ""),
        ("Access-Control-Allow-Origin", ""),
        ("WWW-Authenticate", ""),
        ("Proxy-Authenticate", " \t "),
        ("X-Optional-Metadata", "a\rb"),
        ("X-Optional-Metadata", "a\x7fb"),
    ] {
        let mut wire = Vec::new();
        {
            let mut transport = HttpTransport::new(Cursor::new(Vec::<u8>::new()), &mut wire);
            let response = HttpResponse::ok().with_header(name, value);
            assert!(matches!(
                transport.write_response(&response),
                Err(HttpError::InvalidHeader(_))
            ));
        }
        assert!(wire.is_empty(), "invalid response emitted partial output");
    }
}

#[test]
fn gh332_cors_origin_does_not_become_optional() {
    for origin in ["", " ", "\t", " \t ", "https://example.test\r\nx: y"] {
        let response = HttpResponse::ok().with_cors(origin);
        assert!(!response.headers.contains_key("access-control-allow-origin"));
        assert!(!response.headers.contains_key("vary"));
    }
    let response = HttpResponse::ok().with_cors("https://example.test");
    assert_eq!(
        response
            .headers
            .get("access-control-allow-origin")
            .map(String::as_str),
        Some("https://example.test")
    );
}

#[test]
fn gh332_wire_empty_fields_preserve_consecutive_request_boundaries() {
    let mut wire = wire_request("Tailscale-User-Profile-Pic", "");
    wire.extend(wire_request("X-Optional-Metadata", " \t "));
    let mut transport = HttpTransport::new(Cursor::new(wire), Vec::new());
    let first = transport.read_request().expect("first request");
    let second = transport.read_request().expect("second request");
    assert_eq!(first.header("tailscale-user-profile-pic"), Some(""));
    assert_eq!(second.header("x-optional-metadata"), Some(""));
    assert_eq!(first.body, INITIALIZE.as_bytes());
    assert_eq!(second.body, INITIALIZE.as_bytes());
    assert!(matches!(transport.read_request(), Err(HttpError::Closed)));
}

#[test]
fn gh332_empty_accept_reaches_semantic_negotiation() {
    let handler = HttpRequestHandler::new();
    let request = HttpRequest::new(HttpMethod::Post, "/mcp/v1")
        .with_header("Content-Type", "application/json")
        .with_header("MCP-Protocol-Version", "2026-07-28")
        .with_header("Mcp-Method", "server/discover")
        .with_header("Accept", "")
        .with_header("Tailscale-User-Profile-Pic", "")
        .with_body(br#"{"jsonrpc":"2.0","id":332,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{}}}}"#.to_vec());
    assert!(matches!(
        handler.admit_modern_request(&request),
        Err(HttpError::NotAcceptable)
    ));
    let admitted = request.with_header("Accept", "application/json");
    assert!(handler.admit_modern_request(&admitted).is_ok());
}

#[test]
fn gh332_transport_round_trip_dispatches_initialize_with_empty_extension() {
    use asupersync::Cx;
    use fastmcp_protocol::{JsonRpcMessage, JsonRpcResponse, RequestId};
    use fastmcp_transport::Transport;

    let mut output = Vec::new();
    {
        let mut transport = HttpTransport::new(
            Cursor::new(wire_request("Tailscale-User-Profile-Pic", "")),
            &mut output,
        );
        let cx = Cx::for_testing();
        let message = transport.recv(&cx).expect("initialize reaches dispatch");
        assert!(matches!(message, JsonRpcMessage::Request(ref request)
            if request.method == "initialize" && request.id == Some(RequestId::Number(1))));
        transport
            .send(
                &cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(1),
                    serde_json::json!({"accepted": true}),
                )),
            )
            .expect("response completes the admitted exchange");
    }
    let wire = String::from_utf8(output).expect("UTF-8 response");
    assert!(wire.starts_with("HTTP/1.1 200 OK\r\n"));
    assert!(wire.contains(r#""accepted":true"#));
}
