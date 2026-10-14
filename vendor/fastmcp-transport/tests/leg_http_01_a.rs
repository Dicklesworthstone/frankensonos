#![cfg(feature = "legacy-2024-11-05")]

use std::io::{BufRead, BufReader, Cursor, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, mpsc::sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use asupersync::{Budget, Cx};
use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, RequestId};
use fastmcp_transport::{
    Transport, TransportError,
    http::LegacySseHttpPostSink,
    sse::{LegacySseClientTransport, LegacySseServerTransport, SseEvent},
};

// Compile-probe test binaries can briefly saturate a loaded release host. Keep
// the fixture bounded while leaving enough room for the connected client test
// thread to be rescheduled before its first header write.
const LOOPBACK_PEER_BOUND: Duration = Duration::from_secs(10);

struct ServerThread(Option<thread::JoinHandle<()>>);

impl ServerThread {
    fn spawn(task: impl FnOnce() + Send + 'static) -> Self {
        Self(Some(thread::spawn(task)))
    }

    fn join(mut self) {
        self.0
            .take()
            .expect("server thread is joined at most once")
            .join()
            .expect("the bounded loopback server completes");
    }
}

impl Drop for ServerThread {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

fn accept_loopback(listener: &TcpListener) -> TcpStream {
    listener
        .set_nonblocking(true)
        .expect("the loopback listener becomes boundedly probeable");
    let deadline = Instant::now() + LOOPBACK_PEER_BOUND;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream
                    .set_read_timeout(Some(LOOPBACK_PEER_BOUND))
                    .expect("the loopback peer read is bounded");
                stream
                    .set_write_timeout(Some(LOOPBACK_PEER_BOUND))
                    .expect("the loopback peer write is bounded");
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < deadline,
                    "the bounded loopback server received no expected connection"
                );
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("the bounded loopback accept fails: {error}"),
        }
    }
}

fn read_http_request(stream: TcpStream) -> String {
    let mut reader = BufReader::new(stream);
    let mut request = String::new();
    let mut content_length = 0;
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .expect("the loopback peer sends a complete HTTP header line");
        if line.is_empty() {
            panic!("the loopback peer closed before the HTTP header terminator");
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            content_length = value
                .trim()
                .parse()
                .expect("the loopback POST content length is numeric");
        }
        request.push_str(&line);
        if line == "\r\n" {
            break;
        }
    }
    let mut body = vec![0_u8; content_length];
    reader
        .read_exact(&mut body)
        .expect("the loopback peer sends the declared HTTP body");
    request.push_str(std::str::from_utf8(&body).expect("JSON-RPC body is UTF-8"));
    request
}

fn established_legacy_post_client(
    endpoint: String,
    cx: &Cx,
) -> LegacySseClientTransport<Cursor<Vec<u8>>, LegacySseHttpPostSink> {
    let endpoint_event = SseEvent::endpoint(endpoint.clone())
        .to_bytes()
        .expect("the exact legacy endpoint event frames");
    let mut client =
        LegacySseClientTransport::new(Cursor::new(endpoint_event), LegacySseHttpPostSink::new());
    assert_eq!(
        client
            .establish(cx)
            .expect("the in-memory exact legacy endpoint event establishes"),
        endpoint
    );
    client
}

fn legacy_post_request() -> JsonRpcMessage {
    JsonRpcMessage::Request(JsonRpcRequest::new("tools/list", None, 1_i64))
}

fn rejected_legacy_post(response: Vec<u8>) -> TransportError {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    let endpoint = format!(
        "http://{}/legacy/messages",
        listener
            .local_addr()
            .expect("the listener exposes its loopback address")
    );
    let server = ServerThread::spawn(move || {
        let mut post_socket = accept_loopback(&listener);
        let _post = read_http_request(
            post_socket
                .try_clone()
                .expect("the advertised POST socket clones"),
        );
        post_socket
            .write_all(&response)
            .expect("the bounded response fixture writes");
        post_socket
            .flush()
            .expect("the bounded response fixture flushes");
    });
    let cx = Cx::for_testing();
    let mut client = established_legacy_post_client(endpoint, &cx);
    let error = client
        .send(&cx, &legacy_post_request())
        .expect_err("the malformed or non-accepted POST response is rejected");
    assert!(matches!(
        client.send(&cx, &legacy_post_request()),
        Err(TransportError::Closed)
    ));
    server.join();
    error
}

#[test]
fn leg_http_01_a_positive() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    let authority = listener
        .local_addr()
        .expect("the listener exposes its loopback address")
        .to_string();
    let endpoint = format!("http://{authority}/legacy/messages");
    assert!(
        endpoint.starts_with("http://127.0.0.1:"),
        "the public positive fixes the sink to a numeric IPv4 authority"
    );
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_server = Arc::clone(&captured);
    let endpoint_server = endpoint.clone();
    let server = ServerThread::spawn(move || {
        let sse_socket = accept_loopback(&listener);
        let get = read_http_request(sse_socket.try_clone().expect("the SSE socket clones"));
        let mut legacy_server = LegacySseServerTransport::new(
            sse_socket.try_clone().expect("the SSE socket clones"),
            std::iter::empty::<JsonRpcRequest>(),
            endpoint_server,
        );
        let server_cx = Cx::for_testing();
        legacy_server
            .open(&server_cx)
            .expect("the exact legacy adapter opens with its endpoint event");
        legacy_server
            .send(
                &server_cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(2),
                    serde_json::json!({"from": "legacy-sse"}),
                )),
            )
            .expect("server JSON-RPC stays on the SSE stream");

        let mut post_socket = accept_loopback(&listener);
        let post = read_http_request(
            post_socket
                .try_clone()
                .expect("the advertised POST socket clones"),
        );
        post_socket
            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("the advertised POST receives its exact acceptance response");
        post_socket
            .flush()
            .expect("the advertised POST acceptance response flushes");
        captured_server
            .lock()
            .expect("capture mutex remains available")
            .extend([get, post]);
    });

    let mut sse_socket = TcpStream::connect(&authority).expect("the configured SSE GET connects");
    sse_socket
        .write_all(b"GET /legacy/sse HTTP/1.1\r\nHost: loopback\r\n\r\n")
        .expect("the configured legacy SSE GET writes");
    sse_socket
        .flush()
        .expect("the configured legacy SSE GET flushes");
    sse_socket
        .shutdown(Shutdown::Write)
        .expect("the configured legacy SSE GET completes its request body");
    let mut client = LegacySseClientTransport::new(sse_socket, LegacySseHttpPostSink::new());
    let cx = Cx::for_testing();

    assert_eq!(
        client
            .establish(&cx)
            .expect("first SSE event establishes the endpoint"),
        endpoint
    );
    client
        .send(
            &cx,
            &JsonRpcMessage::Request(JsonRpcRequest::new("tools/list", None, 1_i64)),
        )
        .expect("client JSON-RPC travels only through the advertised POST");
    assert!(matches!(
        client.recv(&cx),
        Ok(JsonRpcMessage::Response(response)) if response.id == Some(RequestId::Number(2))
    ));
    server.join();

    let captured = captured.lock().expect("capture mutex remains available");
    assert!(captured[0].starts_with("GET /legacy/sse HTTP/1.1\r\n"));
    assert!(captured[1].starts_with("POST /legacy/messages HTTP/1.1\r\n"));
    assert!(captured[1].contains("\"method\":\"tools/list\""));
}

#[test]
fn leg_http_01_a_planted_negative() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    let authority = listener
        .local_addr()
        .expect("the listener exposes its loopback address")
        .to_string();
    let endpoint = format!("http://{authority}/legacy/messages");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let captured_server = Arc::clone(&captured);
    let endpoint_server = endpoint.clone();
    let server = ServerThread::spawn(move || {
        let sse_socket = accept_loopback(&listener);
        let get = read_http_request(sse_socket.try_clone().expect("the SSE socket clones"));
        let mut legacy_server = LegacySseServerTransport::new(
            sse_socket.try_clone().expect("the SSE socket clones"),
            std::iter::empty::<JsonRpcRequest>(),
            endpoint_server,
        );
        let server_cx = Cx::for_testing();
        legacy_server
            .open(&server_cx)
            .expect("the exact legacy adapter opens with its endpoint event");
        legacy_server
            .send(
                &server_cx,
                &JsonRpcMessage::Response(JsonRpcResponse::success(
                    RequestId::Number(2),
                    serde_json::json!({"from": "legacy-sse"}),
                )),
            )
            .expect("server JSON-RPC stays on the SSE stream");

        let mut post_socket = accept_loopback(&listener);
        let post = read_http_request(
            post_socket
                .try_clone()
                .expect("the advertised POST socket clones"),
        );
        // RH-5: this is the sole wire-level difference from the positive case.
        post_socket
            .write_all(b"HTTP/1.1 401 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .expect("the planted rejection response writes");
        post_socket
            .flush()
            .expect("the planted rejection response flushes");
        captured_server
            .lock()
            .expect("capture mutex remains available")
            .extend([get, post]);
    });

    let mut sse_socket = TcpStream::connect(&authority).expect("the configured SSE GET connects");
    sse_socket
        .write_all(b"GET /legacy/sse HTTP/1.1\r\nHost: loopback\r\n\r\n")
        .expect("the configured legacy SSE GET writes");
    sse_socket
        .flush()
        .expect("the configured legacy SSE GET flushes");
    sse_socket
        .shutdown(Shutdown::Write)
        .expect("the configured legacy SSE GET completes its request body");
    let mut client = LegacySseClientTransport::new(sse_socket, LegacySseHttpPostSink::new());
    let cx = Cx::for_testing();

    assert!(matches!(
        client.establish(&cx),
        Ok(advertised) if advertised == endpoint
    ));
    assert!(matches!(
        client.send(
            &cx,
            &JsonRpcMessage::Request(JsonRpcRequest::new("tools/list", None, 1_i64)),
        ),
        Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidData
    ));
    assert!(matches!(
        client.send(
            &cx,
            &JsonRpcMessage::Request(JsonRpcRequest::new("tools/list", None, 1_i64)),
        ),
        Err(TransportError::Closed)
    ));
    server.join();

    let captured = captured.lock().expect("capture mutex remains available");
    assert_eq!(
        captured.len(),
        2,
        "the rejected POST is the only server receipt"
    );
    assert!(captured[0].starts_with("GET /legacy/sse HTTP/1.1\r\n"));
    assert!(captured[1].starts_with("POST /legacy/messages HTTP/1.1\r\n"));
    assert!(captured[1].contains("\"method\":\"tools/list\""));
}

#[test]
fn leg_http_01_a_rejects_malformed_redirect_eof_and_oversized_post_heads() {
    let malformed =
        rejected_legacy_post(b"HTTP/1.1 202 Accepted\r\nMalformed-Header\r\n\r\n".to_vec());
    assert!(matches!(
        malformed,
        TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData
    ));

    let redirect = rejected_legacy_post(
        b"HTTP/1.1 302 Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    );
    assert!(matches!(
        redirect,
        TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData
    ));

    let eof = rejected_legacy_post(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n".to_vec());
    assert!(
        matches!(
            &eof,
            TransportError::Io(error) if error.kind() == std::io::ErrorKind::UnexpectedEof
        ),
        "incomplete response head must fail as EOF, got {eof:?}"
    );

    let mut oversized = b"HTTP/1.1 202 Accepted\r\nX-Fill: ".to_vec();
    oversized.extend(vec![b'x'; 16 * 1024]);
    let oversized = rejected_legacy_post(oversized);
    assert!(matches!(
        oversized,
        TransportError::Io(ref error) if error.kind() == std::io::ErrorKind::InvalidData
    ));
}

#[test]
fn leg_http_01_a_hostname_authority_fails_closed_before_contact() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    listener
        .set_nonblocking(true)
        .expect("the no-contact listener becomes probeable");
    let endpoint = format!(
        "http://localhost:{}/legacy/messages",
        listener
            .local_addr()
            .expect("the listener exposes its loopback address")
            .port()
    );
    let cx = Cx::for_testing();
    let mut client = established_legacy_post_client(endpoint, &cx);

    assert!(matches!(
        client.send(&cx, &legacy_post_request()),
        Err(TransportError::Io(ref error)) if error.kind() == std::io::ErrorKind::InvalidInput
    ));
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
    assert!(matches!(
        client.send(&cx, &legacy_post_request()),
        Err(TransportError::Closed)
    ));
}

#[test]
fn leg_http_01_a_post_wait_observes_cancellation_after_request_receipt() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    let endpoint = format!(
        "http://{}/legacy/messages",
        listener
            .local_addr()
            .expect("the listener exposes its loopback address")
    );
    let cx = Cx::for_testing();
    let cancel_cx = cx.clone();
    let server = ServerThread::spawn(move || {
        let post_socket = accept_loopback(&listener);
        let _post = read_http_request(
            post_socket
                .try_clone()
                .expect("the advertised POST socket clones"),
        );
        cancel_cx.set_cancel_requested(true);
    });
    let mut client = established_legacy_post_client(endpoint, &cx);

    assert!(matches!(
        client.send(&cx, &legacy_post_request()),
        Err(TransportError::Cancelled)
    ));
    server.join();
}

#[test]
fn leg_http_01_a_post_wait_observes_caller_deadline_and_latches_closed() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("a loopback listener binds");
    let endpoint = format!(
        "http://{}/legacy/messages",
        listener
            .local_addr()
            .expect("the listener exposes its loopback address")
    );
    let (release_sender, release_receiver) = sync_channel(1);
    let server = ServerThread::spawn(move || {
        let post_socket = accept_loopback(&listener);
        let _post = read_http_request(
            post_socket
                .try_clone()
                .expect("the advertised POST socket clones"),
        );
        // The client deliberately half-closes its write side after the POST,
        // so reading here would observe EOF immediately and accidentally
        // close the response side before the caller deadline. Retain the
        // socket until the caller reports that its bounded send has settled.
        release_receiver
            .recv_timeout(LOOPBACK_PEER_BOUND)
            .expect("the deadline-bounded client releases the withheld-response peer");
        drop(post_socket);
    });
    let established_cx = Cx::for_testing();
    let mut client = established_legacy_post_client(endpoint, &established_cx);
    let deadline_cx = Cx::for_testing_with_budget(
        Budget::new().with_deadline(Cx::for_testing().now() + Duration::from_millis(250)),
    );

    let deadline_result = client.send(&deadline_cx, &legacy_post_request());
    release_sender
        .send(())
        .expect("the withheld-response peer remains available through deadline settlement");
    server.join();
    assert!(
        matches!(deadline_result, Err(TransportError::Timeout)),
        "withheld legacy POST response must exhaust the caller deadline, got {deadline_result:?}"
    );
    assert!(matches!(
        client.send(&established_cx, &legacy_post_request()),
        Err(TransportError::Closed)
    ));
}
