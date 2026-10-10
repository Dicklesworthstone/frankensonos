use asupersync::Cx;
use fastmcp_protocol::protocol_version::{HeaderMismatchReason, RequestAdmissionError};
use fastmcp_protocol::{JsonRpcRequest, JsonRpcResponse, RequestId};
use fastmcp_transport::{
    TransportError,
    http::{
        HttpError, HttpHandlerConfig, HttpMethod, HttpRequest, HttpRequestHandler,
        HttpResponseRepresentation, StreamableHttpTransport,
    },
};

fn modern_sse_request() -> HttpRequest {
    let request = JsonRpcRequest::new(
        "tools/call",
        Some(serde_json::json!({
            "name": "weather",
            "arguments": {},
            "_meta": {
                "io.modelcontextprotocol/protocolVersion": "2026-07-28"
            }
        })),
        901_i64,
    );
    HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("content-type", "application/json")
        .with_header("accept", "text/event-stream")
        .with_header("MCP-Protocol-Version", "2026-07-28")
        .with_header("Mcp-Method", "tools/call")
        .with_header("Mcp-Name", "weather")
        .with_body(serde_json::to_vec(&request).expect("serialize modern request"))
}

fn modern_handler() -> HttpRequestHandler {
    HttpRequestHandler::with_config(HttpHandlerConfig {
        base_path: "/mcp".to_owned(),
        ..HttpHandlerConfig::default()
    })
}

#[test]
fn http_01_a_positive() {
    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let request_id = RequestId::Number(701);
    let response_body = responses
        .for_request(request_id.clone())
        .expect("one public response body is registered for the request");
    let cancellation = response_body.cancellation();
    let cx = Cx::for_testing();

    transport
        .send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(request_id.clone(), serde_json::json!({"response": "bound"})),
        )
        .expect("the request-bound final response is admitted");

    assert_eq!(
        response_body
            .recv_response(&cx)
            .expect("the public response body receives its own final response")
            .id,
        Some(request_id)
    );
    assert!(response_body.is_finished());
    assert!(cancellation.is_cancelled());
    assert_eq!(responses.pending_responses(), 0);
}

#[test]
fn http_01_a_planted_negative() {
    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let request_id = RequestId::Number(702);
    let response_body = responses
        .for_request(request_id.clone())
        .expect("one public response body is registered for the request");
    let cancellation = response_body.cancellation();
    let cx = Cx::for_testing();
    let pending_before = responses.pending_responses();

    // Planted forbidden dimension: only the response body is disconnected
    // before the otherwise identical handler commit.
    drop(response_body);

    assert!(matches!(
        transport.send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(request_id, serde_json::json!({"response": "bound"})),
        ),
        Err(TransportError::Cancelled)
    ));
    assert_eq!(responses.pending_responses(), pending_before);
    assert!(!responses.is_closed());
}

#[test]
fn http_01_b_positive() {
    let mut transport =
        StreamableHttpTransport::with_capacity(1).expect("a one-entry bounded transport is valid");
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let first_id = RequestId::Number(801);
    let first_body = responses
        .for_request(first_id.clone())
        .expect("the first public response body is registered");
    let first_cancellation = first_body.cancellation();
    let second_id = RequestId::Number(802);
    let second_body = responses
        .for_request(second_id.clone())
        .expect("the second public response body is registered");
    let second_cancellation = second_body.cancellation();
    let cx = Cx::for_testing();

    transport
        .send_response_for_request(
            &cx,
            &first_cancellation,
            JsonRpcResponse::success(first_id.clone(), serde_json::json!({"sequence": 1})),
        )
        .expect("the first response consumes the sole bounded slot");
    assert!(first_cancellation.is_terminal_committed());
    assert!(matches!(
        transport.send_response_for_request(
            &cx,
            &second_cancellation,
            JsonRpcResponse::success(second_id.clone(), serde_json::json!({"sequence": 2})),
        ),
        Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert_eq!(responses.pending_responses(), 1);

    assert_eq!(
        first_body
            .recv_response(&cx)
            .expect("draining the first final response releases the bounded slot")
            .id,
        Some(first_id.clone())
    );
    assert!(matches!(
        transport.send_response_for_request(
            &cx,
            &first_cancellation,
            JsonRpcResponse::success(first_id, serde_json::json!({"late": true})),
        ),
        Err(TransportError::Cancelled)
    ));
    transport
        .send_response_for_request(
            &cx,
            &second_cancellation,
            JsonRpcResponse::success(second_id.clone(), serde_json::json!({"sequence": 2})),
        )
        .expect("the released slot admits the second response");
    assert_eq!(
        second_body
            .recv_response(&cx)
            .expect("the second public response body receives its final response")
            .id,
        Some(second_id)
    );
    assert_eq!(responses.pending_responses(), 0);
}

#[test]
fn http_01_b_planted_negative() {
    let mut transport =
        StreamableHttpTransport::with_capacity(1).expect("a one-entry bounded transport is valid");
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let first_id = RequestId::Number(801);
    let first_body = responses
        .for_request(first_id.clone())
        .expect("the first public response body is registered");
    let first_cancellation = first_body.cancellation();
    let second_id = RequestId::Number(802);
    let second_body = responses
        .for_request(second_id.clone())
        .expect("the second public response body is registered");
    let second_cancellation = second_body.cancellation();
    let cx = Cx::for_testing();
    let pending_before = responses.pending_responses();

    // Planted forbidden dimension: only the first response body is disconnected
    // before the otherwise identical bounded final-response commit.
    drop(first_body);

    assert!(matches!(
        transport.send_response_for_request(
            &cx,
            &first_cancellation,
            JsonRpcResponse::success(first_id, serde_json::json!({"sequence": 1})),
        ),
        Err(TransportError::Cancelled)
    ));
    assert_eq!(responses.pending_responses(), pending_before);
    assert!(!responses.is_closed());
    transport
        .send_response_for_request(
            &cx,
            &second_cancellation,
            JsonRpcResponse::success(second_id.clone(), serde_json::json!({"sequence": 2})),
        )
        .expect("the cancelled first body leaves the bounded slot available");
    assert_eq!(
        second_body
            .recv_response(&cx)
            .expect("the independent second request remains live")
            .id,
        Some(second_id)
    );
}

#[test]
fn http_02_a_positive() {
    let handler = modern_handler();
    let admission = handler
        .admit_modern_request(&modern_sse_request())
        .expect("the public modern HTTP boundary admits a matching 2026 request");
    assert_eq!(
        admission.response_representation(),
        HttpResponseRepresentation::Sse
    );
    assert_eq!(admission.request().method, "tools/call");

    let mut transport = StreamableHttpTransport::with_capacity(1)
        .expect("one bounded response slot is a valid public configuration");
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("idle response registry is observable"),
        0
    );

    let response_body = admission
        .bind_sse_response_body(&responses)
        .expect("only the admitted request receives an SSE response body");
    let cancellation = response_body.cancellation();
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("bound response body is observable"),
        1
    );

    let cx = Cx::for_testing();
    transport
        .send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(
                RequestId::Number(901),
                serde_json::json!({"forecast": "clear"}),
            ),
        )
        .expect("the request-owned SSE body admits one terminal response");
    assert_eq!(responses.pending_responses(), 1);
    assert_eq!(
        response_body
            .recv_response(&cx)
            .expect("the SSE body consumes its own terminal response")
            .id,
        Some(RequestId::Number(901))
    );
    assert!(response_body.is_finished());
    assert!(cancellation.is_cancelled());
    assert_eq!(responses.pending_responses(), 0);
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("finished response body is released"),
        0
    );
}

#[test]
fn http_02_a_planted_negative() {
    let handler = modern_handler();
    let baseline = modern_sse_request();
    let mut rejected = baseline.clone();
    // Planted forbidden dimension: only the required name mirror changes.
    rejected
        .headers
        .insert("mcp-name".to_owned(), "other-weather".to_owned());

    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");
    let before_pending = responses.pending_responses();
    let before_bodies = responses
        .live_request_bodies()
        .expect("idle response registry is observable");

    let error = handler
        .admit_modern_request(&rejected)
        .expect_err("changing only Mcp-Name must reject at the PRT-03 boundary");
    assert!(matches!(
        error,
        HttpError::ProtocolAdmission(RequestAdmissionError::HeaderMismatch(error))
            if error.reason() == HeaderMismatchReason::HeaderBodyNameMismatch
    ));
    assert_eq!(rejected.method, baseline.method);
    assert_eq!(rejected.path, baseline.path);
    assert_eq!(rejected.body, baseline.body);
    assert_eq!(responses.pending_responses(), before_pending);
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("rejection cannot allocate a response body"),
        before_bodies
    );
    assert_eq!(
        handler
            .admit_modern_request(&baseline)
            .expect("the unchanged baseline remains freshly admissible")
            .response_representation(),
        HttpResponseRepresentation::Sse
    );
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("fresh baseline admission has not opened a response body"),
        before_bodies
    );
}

#[test]
fn http_02_b_positive() {
    let handler = modern_handler();
    let admission = handler
        .admit_modern_request(&modern_sse_request())
        .expect("a matching request is admitted before its response body opens");
    let mut transport = StreamableHttpTransport::with_capacity(1)
        .expect("one bounded response slot is a valid public configuration");
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");
    let response_body = admission
        .bind_sse_response_body(&responses)
        .expect("the request owns exactly one finite response body");
    let cancellation = response_body.cancellation();
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("bound response body is observable"),
        1
    );

    drop(response_body);

    assert!(cancellation.is_cancelled());
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("dropped response body is released"),
        0
    );
    assert_eq!(responses.pending_responses(), 0);
}

#[test]
fn http_02_b_planted_negative() {
    let handler = modern_handler();
    let baseline = modern_sse_request();
    let mut rejected = baseline.clone();
    // Planted forbidden dimension: only the response representation changes.
    rejected
        .headers
        .insert("accept".to_owned(), "application/xml".to_owned());

    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");
    let before_pending = responses.pending_responses();
    let before_bodies = responses
        .live_request_bodies()
        .expect("idle response registry is observable");

    assert!(matches!(
        handler.admit_modern_request(&rejected),
        Err(HttpError::NotAcceptable)
    ));
    assert_eq!(rejected.method, baseline.method);
    assert_eq!(rejected.path, baseline.path);
    assert_eq!(rejected.body, baseline.body);
    assert_eq!(responses.pending_responses(), before_pending);
    assert_eq!(
        responses
            .live_request_bodies()
            .expect("rejection cannot allocate a response body"),
        before_bodies
    );
    assert_eq!(
        handler
            .admit_modern_request(&baseline)
            .expect("the unchanged SSE baseline remains freshly admissible")
            .response_representation(),
        HttpResponseRepresentation::Sse
    );
}

#[test]
fn http_01_i_positive() {
    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let request_id = RequestId::Number(991);
    let response_body = responses
        .for_request(request_id.clone())
        .expect("one public response body is registered for the request");
    let cancellation = response_body.cancellation();
    let cx = Cx::for_testing();

    transport
        .send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(
                request_id.clone(),
                serde_json::json!({"status": "integrated"}),
            ),
        )
        .expect("the request-bound final response is admitted");

    assert_eq!(
        response_body
            .recv_response(&cx)
            .expect("the public response body receives its own final response")
            .id,
        Some(request_id)
    );
    assert!(response_body.is_finished());
    assert!(cancellation.is_cancelled());
    assert_eq!(responses.pending_responses(), 0);
}

#[test]
fn http_01_i_planted_negative() {
    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("the public HTTP response stream can be externalized once");
    let request_id = RequestId::Number(992);
    let response_body = responses
        .for_request(request_id.clone())
        .expect("one public response body is registered for the request");
    let cancellation = response_body.cancellation();
    let cx = Cx::for_testing();
    let pending_before = responses.pending_responses();

    // Planted forbidden dimension: disconnected body before commit
    drop(response_body);

    assert!(matches!(
        transport.send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(request_id, serde_json::json!({"status": "integrated"})),
        ),
        Err(TransportError::Cancelled)
    ));
    assert_eq!(responses.pending_responses(), pending_before);
    assert!(!responses.is_closed());
}

#[test]
fn http_02_i_positive() {
    http_02_integration_positive();
}

#[test]
fn http_02_integration_positive() {
    let handler = modern_handler();
    let admission = handler
        .admit_modern_request(&modern_sse_request())
        .expect("the public modern HTTP boundary admits a matching 2026 request");
    assert_eq!(
        admission.response_representation(),
        HttpResponseRepresentation::Sse
    );

    let mut transport = StreamableHttpTransport::with_capacity(2)
        .expect("bounded response slot is valid public configuration");
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");

    let response_body = admission
        .bind_sse_response_body(&responses)
        .expect("admitted request receives an SSE response body");
    let cancellation = response_body.cancellation();
    let cx = Cx::for_testing();

    transport
        .send_response_for_request(
            &cx,
            &cancellation,
            JsonRpcResponse::success(
                RequestId::Number(901),
                serde_json::json!({"forecast": "sunny"}),
            ),
        )
        .expect("the request-owned SSE body admits terminal response");

    assert_eq!(
        response_body
            .recv_response(&cx)
            .expect("the SSE body consumes its own terminal response")
            .id,
        Some(RequestId::Number(901))
    );
    assert!(response_body.is_finished());
    assert!(cancellation.is_cancelled());
    assert_eq!(responses.pending_responses(), 0);
}

#[test]
fn http_02_i_planted_negative() {
    http_02_integration_planted_negative();
}

#[test]
fn http_02_integration_planted_negative() {
    let handler = modern_handler();
    let baseline = modern_sse_request();
    let mut rejected = baseline.clone();
    // Planted forbidden dimension: unsupported protocol version 2025-11-25
    rejected
        .headers
        .insert("mcp-protocol-version".to_owned(), "2025-11-25".to_owned());

    let mut transport = StreamableHttpTransport::new();
    let responses = transport
        .response_stream()
        .expect("one public response stream can be externalized");
    let before_pending = responses.pending_responses();

    let error = handler
        .admit_modern_request(&rejected)
        .expect_err("unsupported 2025-11-25 version must reject at protocol admission boundary");
    assert!(matches!(
        error,
        HttpError::ProtocolAdmission(RequestAdmissionError::HeaderMismatch(err))
            if err.reason() == HeaderMismatchReason::HeaderBodyVersionMismatch
    ));
    assert_eq!(rejected.method, baseline.method);
    assert_eq!(rejected.path, baseline.path);
    assert_eq!(responses.pending_responses(), before_pending);
}
