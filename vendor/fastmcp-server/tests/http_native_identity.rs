//! Native HTTP identity custody through the shipped JSON and SSE listener paths.
//! The resolver deliberately treats a test-only header as trusted. Production
//! applications must establish their own trusted proxy or peer boundary.
#![recursion_limit = "256"]

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asupersync::Cx;
use asupersync::io::{AsyncReadExt, AsyncWriteExt};
use asupersync::types::CancelKind;
use fastmcp_core::{AuthContext, McpContext, McpError, McpResult};
use fastmcp_protocol::{Content, FINAL_PROTOCOL_VERSION, Tool, protocol_policy::ProtocolPolicy};
use fastmcp_server::{
    HttpServerConfig, HttpServerShutdown, Server, StaticTokenVerifier, TokenAuthProvider,
    ToolHandler,
};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpResponse, HttpStatus};
use serde_json::{Value, json};

const TOOL: &str = "native_identity_probe";

#[derive(Clone, Default)]
struct Probe(Arc<AtomicUsize>);

impl ToolHandler for Probe {
    fn definition(&self) -> Tool {
        Tool {
            name: TOOL.to_owned(),
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: Vec::new(),
            annotations: None,
        }
    }

    fn call(&self, context: &McpContext, _: Value) -> McpResult<Vec<Content>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![Content::text(
            json!({
                "subject": context.auth().and_then(|auth| auth.subject),
            })
            .to_string(),
        )])
    }
}

fn server(config: HttpServerConfig, probe: Probe) -> Server {
    Server::new("native-identity", "1")
        .protocol_policy(ProtocolPolicy::ModernOnly)
        .unwrap()
        .http_config(config)
        .tool(probe)
        .build()
}

fn call(subject: Option<&str>, sse: bool) -> HttpRequest {
    let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("host", "identity.test")
        .with_header("content-type", "application/json")
        .with_header(
            "accept",
            if sse {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .with_header("mcp-protocol-version", FINAL_PROTOCOL_VERSION)
        .with_header("mcp-method", "tools/call")
        .with_header("mcp-name", TOOL)
        .with_body(
            serde_json::to_vec(&json!({
                "jsonrpc": "2.0", "id": 17, "method": "tools/call",
                "params": {
                    "name": TOOL, "arguments": {"subject": "forged-argument"},
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": FINAL_PROTOCOL_VERSION,
                        "io.modelcontextprotocol/clientCapabilities": {},
                        "com.example/subject": "forged-metadata",
                    },
                },
            }))
            .unwrap(),
        );
    if let Some(subject) = subject {
        request = request.with_header("x-test-subject", subject);
    }
    request
}

fn subject(response: &HttpResponse) -> Value {
    assert_eq!(response.status, HttpStatus::OK);
    let message: Value = if response
        .headers
        .get("content-type")
        .is_some_and(|value| value.starts_with("text/event-stream"))
    {
        let body = String::from_utf8(response.body.clone())
            .unwrap()
            .replace("\r\n", "\n");
        let messages: Vec<Value> = body
            .split("\n\n")
            .filter_map(|event| {
                let data = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect::<Vec<_>>()
                    .join("\n");
                if data.is_empty() {
                    None
                } else {
                    Some(serde_json::from_str(&data).unwrap())
                }
            })
            .filter(|message: &Value| message.get("id") == Some(&json!(17)))
            .collect();
        assert_eq!(messages.len(), 1);
        messages.into_iter().next().unwrap()
    } else {
        serde_json::from_slice(&response.body).unwrap()
    };
    assert!(message.get("error").is_none(), "{message}");
    let result: Value =
        serde_json::from_str(message["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    result["subject"].clone()
}

fn run<F, Fut>(scenario: F)
where
    F: FnOnce(Cx) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
        .blocking_threads(1, 4)
        .build()
        .unwrap()
        .block_on(async move {
            let parent = Cx::current().unwrap();
            let mut task = parent.spawn(scenario).unwrap();
            task.join(&parent).await.unwrap();
        });
}

async fn exchange(
    cx: &Cx,
    address: SocketAddr,
    request: HttpRequest,
) -> Result<(SocketAddr, Vec<u8>), String> {
    asupersync::time::timeout(cx.now(), Duration::from_secs(10), async move {
        let mut stream = asupersync::net::TcpStream::connect(address)
            .await
            .map_err(|error| error.to_string())?;
        let peer = stream.local_addr().map_err(|error| error.to_string())?;
        let mut bytes =
            format!("{} {} HTTP/1.1\r\n", request.method.as_str(), request.path).into_bytes();
        for (name, value) in request.headers {
            bytes.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
        }
        bytes.extend_from_slice(
            format!(
                "content-length: {}\r\nconnection: close\r\n\r\n",
                request.body.len(),
            )
            .as_bytes(),
        );
        bytes.extend_from_slice(&request.body);
        stream
            .write_all(&bytes)
            .await
            .map_err(|error| error.to_string())?;
        stream.flush().await.map_err(|error| error.to_string())?;
        let mut response = Vec::new();
        let mut chunk = [0_u8; 4096];
        loop {
            let count = stream
                .read(&mut chunk)
                .await
                .map_err(|error| error.to_string())?;
            if count == 0 {
                break;
            }
            if response.len() + count > 256 * 1024 {
                return Err("wire response exceeded test bound".to_owned());
            }
            response.extend_from_slice(&chunk[..count]);
        }
        Ok((peer, response))
    })
    .await
    .map_err(|_| "wire exchange timed out".to_owned())?
}

fn decode_wire(bytes: &[u8]) -> HttpResponse {
    let head_end = bytes
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap();
    let head = std::str::from_utf8(&bytes[..head_end]).unwrap();
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse::<u16>()
        .unwrap();
    let mut response = HttpResponse::new(HttpStatus(status));
    response.headers.clear();
    for line in lines {
        let (name, value) = line.split_once(':').unwrap();
        assert!(
            response
                .headers
                .insert(name.to_ascii_lowercase(), value.trim().to_owned())
                .is_none()
        );
    }
    let mut body = &bytes[head_end + 4..];
    if response
        .headers
        .get("transfer-encoding")
        .is_some_and(|value| value.eq_ignore_ascii_case("chunked"))
    {
        loop {
            let end = body
                .windows(2)
                .position(|window| window == b"\r\n")
                .unwrap();
            let size =
                usize::from_str_radix(std::str::from_utf8(&body[..end]).unwrap(), 16).unwrap();
            body = &body[end + 2..];
            if size == 0 {
                assert_eq!(body, b"\r\n");
                break;
            }
            assert!(size <= body.len().saturating_sub(2));
            response.body.extend_from_slice(&body[..size]);
            assert_eq!(&body[size..size + 2], b"\r\n");
            body = &body[size + 2..];
        }
    } else {
        response.body = body.to_vec();
    }
    response
}

async fn wire_batch(
    cx: &Cx,
    server: Server,
    requests: Vec<HttpRequest>,
) -> Vec<(SocketAddr, HttpResponse)> {
    let bound = Box::pin(server.bind_http(cx, "127.0.0.1:0")).await.unwrap();
    let address = bound.local_addr().unwrap();
    let (sender, mut receiver) = asupersync::channel::oneshot::channel();
    let mut serving = cx
        .spawn(move |server_cx| async move {
            let _ = sender.send_blocking(server_cx.clone());
            Box::pin(bound.serve(&server_cx)).await
        })
        .unwrap();
    let server_cx = receiver.recv(cx).await.unwrap();
    let result = async {
        let mut responses = Vec::new();
        for request in requests {
            responses.push(exchange(cx, address, request).await?);
        }
        Ok::<_, String>(responses)
    }
    .await;
    server_cx.cancel_with(CancelKind::User, Some("native identity scenario complete"));
    let shutdown = serving.join(cx).await.unwrap().unwrap();
    if let HttpServerShutdown::Nonquiescent(shutdown) = shutdown {
        shutdown.settle(cx).await.unwrap();
    }
    result
        .unwrap()
        .into_iter()
        .map(|(peer, bytes)| (peer, decode_wire(&bytes)))
        .collect()
}

#[test]
fn socket_json_and_sse_resolve_actual_peer_per_request_without_identity_leakage() {
    run(|cx| async move {
        let peers = Arc::new(Mutex::new(Vec::new()));
        let observed = Arc::clone(&peers);
        let config = HttpServerConfig::new().identity_resolver(move |peer, headers| {
            observed.lock().unwrap().push(peer);
            assert!(
                headers
                    .iter()
                    .all(|(name, _)| name == &name.to_ascii_lowercase())
            );
            Ok(AuthContext::with_subject(
                headers
                    .iter()
                    .find(|(name, _)| name == "x-test-subject")
                    .map_or("unknown", |(_, value)| value.as_str()),
            ))
        });
        let probe = Probe::default();
        let responses = wire_batch(
            &cx,
            server(config, probe.clone()),
            vec![
                call(Some("alice"), false),
                call(Some("bob"), true),
                call(None, false),
                call(Some("alice"), true),
            ],
        )
        .await;
        let recorded = peers.lock().unwrap();
        assert_eq!(
            *recorded,
            responses.iter().map(|(peer, _)| *peer).collect::<Vec<_>>()
        );
        assert_eq!(
            responses
                .iter()
                .map(|(_, response)| subject(response))
                .collect::<Vec<_>>(),
            [
                json!("alice"),
                json!("bob"),
                json!("unknown"),
                json!("alice")
            ]
        );
        assert_eq!(probe.0.load(Ordering::SeqCst), 4);
    });
}

#[test]
fn socket_resolver_failure_bearer_and_duplicate_identity_headers_do_not_dispatch() {
    run(|cx| async move {
        let probe = Probe::default();
        let config = HttpServerConfig::new().identity_resolver(|_, headers| {
            if headers
                .iter()
                .any(|(name, value)| name == "x-test-subject" && value == "reject")
            {
                return Err(McpError::internal_error("identity lookup unavailable"));
            }
            Ok(AuthContext::with_subject("alice"))
        });
        let mut duplicate = call(Some("alice"), false);
        duplicate
            .headers
            .insert("X-Test-Subject".to_owned(), "bob".to_owned());
        let mut duplicate_authorization =
            call(Some("alice"), false).with_header("authorization", "Bearer first");
        duplicate_authorization
            .headers
            .insert("Authorization".to_owned(), "Bearer second".to_owned());
        let mut query_token = call(Some("alice"), false);
        query_token.path = "/mcp?access_token=forged".to_owned();
        let mut in_band = call(Some("alice"), false);
        let mut body: Value = serde_json::from_slice(&in_band.body).unwrap();
        body["params"]["token"] = json!("forged-token");
        in_band.body = serde_json::to_vec(&body).unwrap();
        let responses = wire_batch(
            &cx,
            server(config, probe.clone()),
            vec![
                call(Some("alice"), false),
                call(Some("reject"), true),
                call(Some("alice"), false).with_header("authorization", "Bearer alternate"),
                duplicate,
                in_band,
                query_token,
                duplicate_authorization,
                call(Some("alice"), true),
            ],
        )
        .await;
        assert_eq!(subject(&responses[0].1), json!("alice"));
        assert_eq!(subject(&responses[7].1), json!("alice"));
        for (index, status) in [(1, 403), (2, 403), (3, 400), (4, 403), (5, 403), (6, 403)] {
            assert_eq!(responses[index].1.status.0, status);
            assert!(!responses[index].1.headers.contains_key("www-authenticate"));
        }
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
    });
}

#[test]
fn socket_without_resolver_preserves_anonymous_and_bearer_authentication() {
    run(|cx| async move {
        let probe = Probe::default();
        let anonymous = wire_batch(
            &cx,
            server(HttpServerConfig::new(), probe.clone()),
            vec![call(None, false)],
        )
        .await;
        assert_eq!(subject(&anonymous[0].1), Value::Null);
        let verifier =
            StaticTokenVerifier::new([("old-token", AuthContext::with_subject("existing-bearer"))])
                .unwrap();
        let authenticated = Server::new("existing-bearer", "1")
            .protocol_policy(ProtocolPolicy::ModernOnly)
            .unwrap()
            .auth_provider(TokenAuthProvider::new(verifier))
            .tool(probe.clone())
            .build();
        let responses = wire_batch(
            &cx,
            authenticated,
            vec![call(None, true).with_header("authorization", "Bearer old-token")],
        )
        .await;
        assert_eq!(subject(&responses[0].1), json!("existing-bearer"));
        assert_eq!(probe.0.load(Ordering::SeqCst), 2);
    });
}
