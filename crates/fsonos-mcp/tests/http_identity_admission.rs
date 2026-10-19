//! Slow native identity lookups must leave the HTTP reactor responsive.
//!
//! Both listeners share one I/O thread. Real sockets exercise the public MCP
//! boundary without vendor test-internals, speakers, or external identity tools.
#![recursion_limit = "256"]

use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use asupersync::{CancelKind, Cx};
use fastmcp::{FINAL_CLIENT_CAPABILITIES_META_KEY, FINAL_PROTOCOL_VERSION_META_KEY};
use fsonos_api::Identity;
use fsonos_api::identity::SERVE_LOGIN;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_mcp::tools::{Backend, install};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::future::{Future, poll_fn};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::task::Poll;
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2024-11-05";
const HOLD: &str = "x-test-hold-identity";
const IDENTITY_LIMIT: usize = 8;
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY: usize = 256 * 1024;

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("reading the caller's policy must not contact speakers")
    }
}

fn backend() -> Backend {
    let policy = Policy::from_toml(
        r#"
[clients."alice@example.com"]
allow = ["get_policy"]

[clients."bob@example.com"]
allow = ["get_policy"]

[clients."loopback-http"]
allow = []

[clients."unknown"]
allow = []
"#,
    )
    .unwrap();
    Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("reading the caller's policy must not survey the LAN")),
        policy,
        Client::Unknown,
        Box::new(SystemClock),
    )
}

struct Gate {
    io_thread: ThreadId,
    calls: AtomicUsize,
    entered: mpsc::Sender<()>,
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn wait(&self) {
        assert_ne!(
            thread::current().id(),
            self.io_thread,
            "the native resolver must run outside the I/O thread",
        );
        self.entered.send(()).unwrap();
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), TIMEOUT * 3, |open| !*open)
            .unwrap();
        assert!(*open, "the controller releases the held identity lookups");
    }

    fn release(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

/// A deadline covers the entire response, including persistent SSE framing.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    reader: BufReader<TcpStream>,
    deadline: Instant,
    remaining: usize,
    chunked: bool,
    finished: bool,
    events: Vec<u8>,
}

impl Reply {
    fn open(
        address: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> Self {
        Self::sent(Self::send(address, method, path, headers, body))
    }

    fn send(
        address: SocketAddr,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: &str,
    ) -> TcpStream {
        let mut socket =
            TcpStream::connect_timeout(&address, TIMEOUT).expect("connect to test HTTP");
        socket.set_write_timeout(Some(TIMEOUT)).unwrap();
        socket.set_read_timeout(Some(TIMEOUT)).unwrap();
        let connection = if method == "GET" {
            "keep-alive"
        } else {
            "close"
        };
        let mut request = format!(
            "{method} {path} HTTP/1.1\r\nHost: {address}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: {connection}\r\n",
            body.len()
        );
        for (name, value) in headers {
            write!(request, "{name}: {value}\r\n").unwrap();
        }
        request.push_str("\r\n");
        request.push_str(body);
        socket
            .write_all(request.as_bytes())
            .expect("write HTTP request");
        socket.flush().unwrap();
        socket
    }

    fn sent(socket: TcpStream) -> Self {
        let mut reply = Self {
            status: 0,
            headers: Vec::new(),
            reader: BufReader::new(socket),
            deadline: Instant::now() + TIMEOUT,
            remaining: 0,
            chunked: false,
            finished: false,
            events: Vec::new(),
        };
        let status = reply.line();
        reply.status = status.split_whitespace().nth(1).unwrap().parse().unwrap();
        for _ in 0..64 {
            let line = reply.line();
            if line == "\r\n" {
                reply.chunked = reply
                    .header("transfer-encoding")
                    .is_some_and(|value| value.eq_ignore_ascii_case("chunked"));
                reply.remaining = reply
                    .header("content-length")
                    .map_or(0, |value| value.parse().unwrap());
                assert!(reply.remaining <= MAX_REPLY);
                return reply;
            }
            let (name, value) = line.split_once(':').expect("HTTP response header");
            reply
                .headers
                .push((name.to_owned(), value.trim().to_owned()));
        }
        panic!("HTTP response exceeded the header limit");
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find_map(|(key, value)| key.eq_ignore_ascii_case(name).then_some(value.as_str()))
    }

    fn before_read(&self) {
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .expect("HTTP response exceeded its absolute deadline");
        self.reader
            .get_ref()
            .set_read_timeout(Some(remaining))
            .unwrap();
    }

    fn line(&mut self) -> String {
        self.before_read();
        let mut line = String::new();
        assert!(self.reader.read_line(&mut line).expect("read HTTP line") > 0);
        assert!(line.len() <= MAX_REPLY);
        line
    }

    fn bytes(&mut self, length: usize) -> Vec<u8> {
        assert!(length <= MAX_REPLY);
        self.before_read();
        let mut data = vec![0; length];
        self.reader.read_exact(&mut data).expect("read HTTP body");
        data
    }

    fn chunk(&mut self) -> Option<Vec<u8>> {
        if self.finished {
            return None;
        }
        if !self.chunked {
            self.finished = true;
            return (self.remaining > 0).then(|| self.bytes(self.remaining));
        }
        let line = self.line();
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
        if size == 0 {
            assert_eq!(self.line(), "\r\n", "test server emits no HTTP trailers");
            self.finished = true;
            return None;
        }
        let data = self.bytes(size);
        assert_eq!(self.bytes(2), b"\r\n");
        Some(data)
    }

    fn json(&mut self) -> Value {
        let mut body = Vec::new();
        while let Some(chunk) = self.chunk() {
            body.extend_from_slice(&chunk);
            assert!(body.len() <= MAX_REPLY);
        }
        serde_json::from_slice(&body).expect("JSON-RPC HTTP response")
    }

    fn event(&mut self) -> (String, String) {
        assert_eq!(self.status, 200);
        assert!(
            self.header("content-type")
                .is_some_and(|value| value.starts_with("text/event-stream")),
            "{:?}",
            self.headers
        );
        loop {
            if let Some((offset, width)) = self
                .events
                .windows(2)
                .position(|part| part == b"\n\n")
                .map(|at| (at, 2))
                .or_else(|| {
                    self.events
                        .windows(4)
                        .position(|part| part == b"\r\n\r\n")
                        .map(|at| (at, 4))
                })
            {
                let raw: Vec<u8> = self.events.drain(..offset + width).collect();
                let text = std::str::from_utf8(&raw).unwrap();
                let mut kind = String::new();
                let mut data = Vec::new();
                for line in text.lines() {
                    if let Some(value) = line.strip_prefix("event:") {
                        value.trim_start().clone_into(&mut kind);
                    }
                    if let Some(value) = line.strip_prefix("data:") {
                        data.push(value.strip_prefix(' ').unwrap_or(value));
                    }
                }
                if !data.is_empty() {
                    return (kind, data.join("\n"));
                }
                self.before_read();
                continue;
            }
            let chunk = self.chunk().expect("SSE closed before its expected event");
            self.events.extend_from_slice(&chunk);
            assert!(self.events.len() <= MAX_REPLY);
        }
    }

    fn message(&mut self) -> Value {
        assert_eq!(
            self.status, 200,
            "native request must reach JSON-RPC dispatch"
        );
        if self
            .header("content-type")
            .is_some_and(|v| v.starts_with("text/event-stream"))
        {
            let (event, data) = self.event();
            assert_eq!(event, "message");
            serde_json::from_str(&data).expect("SSE JSON-RPC message")
        } else {
            self.json()
        }
    }
}

fn modern_socket(address: SocketAddr, id: i64, subject: &str, hold: bool) -> TcpStream {
    let mut metadata = json!({});
    metadata[FINAL_PROTOCOL_VERSION_META_KEY] = json!(MODERN);
    metadata[FINAL_CLIENT_CAPABILITIES_META_KEY] = json!({});
    let body = json!({
        "jsonrpc": "2.0", "id": id, "method": "tools/call",
        "params": {
            "name": "get_policy", "arguments": {},
            "_meta": metadata,
        },
    });
    Reply::send(
        address,
        "POST",
        "/mcp",
        &[
            ("Accept", "application/json, text/event-stream"),
            ("MCP-Protocol-Version", MODERN),
            ("Mcp-Method", "tools/call"),
            ("Mcp-Name", "get_policy"),
            (SERVE_LOGIN, subject),
            (HOLD, if hold { "yes" } else { "no" }),
        ],
        &body.to_string(),
    )
}

fn assert_policy(message: &Value, id: i64, subject: &str, modern: bool) {
    assert_eq!(message["id"], id, "{message}");
    assert!(message.get("error").is_none(), "{message}");
    let result = message.get("result").expect("successful policy response");
    assert_ne!(result["isError"], true, "{message}");
    assert!(
        result["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains(&format!("You ({subject}):")),
        "request identity must survive the held lookup: {message}",
    );
    if modern {
        assert_eq!(result["structuredContent"]["you"], subject, "{message}");
    }
}

fn overloaded(address: SocketAddr, id: i64) {
    let mut reply = Reply::sent(modern_socket(address, id, ALICE, false));
    assert_eq!(reply.status, 503, "a full identity queue must fail closed");
    assert!(
        reply.chunk().is_none(),
        "admission errors have no identity payload"
    );
}

fn legacy_post(address: SocketAddr, path: &str, subject: &str, body: &Value) -> u16 {
    let mut reply = Reply::open(
        address,
        "POST",
        path,
        &[(SERVE_LOGIN, subject)],
        &body.to_string(),
    );
    while reply.chunk().is_some() {}
    reply.status
}

fn exercise_legacy_owner(address: SocketAddr, mut stream: Reply) {
    let (event, endpoint) = stream.event();
    assert_eq!(event, "endpoint");
    let path = endpoint.strip_prefix(&format!("http://{address}")).unwrap();
    assert!(path.starts_with("/messages?session_id="));
    let initialize = json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": LEGACY, "capabilities": {},
            "clientInfo": {"name": "bounded-native-identity", "version": "1"},
        },
    });
    // Both principals may read policy. Only the opener may use this session,
    // including when a foreign request tries to initialize it first.
    assert_eq!(legacy_post(address, path, BOB, &initialize), 400);
    assert_eq!(legacy_post(address, path, ALICE, &initialize), 202);
    stream.deadline = Instant::now() + TIMEOUT;
    let initialized = stream.message();
    assert_eq!(initialized["id"], 1, "{initialized}");
    assert_eq!(
        initialized["result"]["protocolVersion"], LEGACY,
        "{initialized}"
    );
    assert_eq!(
        legacy_post(
            address,
            path,
            ALICE,
            &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
        ),
        202,
    );
    let policy = json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/call",
        "params": {"name": "get_policy", "arguments": {}},
    });
    assert_eq!(legacy_post(address, path, BOB, &policy), 400);
    assert_eq!(legacy_post(address, path, ALICE, &policy), 202);
    stream.deadline = Instant::now() + TIMEOUT;
    assert_policy(&stream.message(), 2, ALICE, false);
}

struct StopServers {
    cx: Cx,
    addresses: [SocketAddr; 2],
    gate: Arc<Gate>,
}

impl Drop for StopServers {
    fn drop(&mut self) {
        // Also release callbacks on assertion failure so this test cannot
        // strand blocking pool work or mask its original failure at shutdown.
        self.gate.release();
        self.cx.cancel_with(
            CancelKind::User,
            Some("identity admission assertions finished"),
        );
        for address in self.addresses {
            let _ = TcpStream::connect_timeout(&address, Duration::from_secs(1));
        }
    }
}

fn exercise(slow: SocketAddr, independent: SocketAddr, gate: &Gate, entered: &mpsc::Receiver<()>) {
    let mut held = Vec::new();
    for id in 0..7 {
        let subject = if id % 2 == 0 { ALICE } else { BOB };
        held.push((id, subject, modern_socket(slow, id, subject, true)));
    }
    let legacy = Reply::send(
        slow,
        "GET",
        "/sse",
        &[
            ("Accept", "text/event-stream"),
            (SERVE_LOGIN, ALICE),
            (HOLD, "yes"),
        ],
        "",
    );
    for _ in 0..IDENTITY_LIMIT {
        entered
            .recv_timeout(TIMEOUT)
            .expect("all identity callbacks must enter without blocking the I/O reactor");
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), IDENTITY_LIMIT);
    overloaded(slow, 80);
    assert_eq!(gate.calls.load(Ordering::SeqCst), IDENTITY_LIMIT);

    // A disconnected request still owns its running synchronous callback.
    let (_, _, abandoned) = held.remove(0);
    abandoned.shutdown(Shutdown::Both).unwrap();
    drop(abandoned);
    overloaded(slow, 81);
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        IDENTITY_LIMIT,
        "disconnect and overload must not release a running callback's slot",
    );

    let mut responsive = Reply::sent(modern_socket(independent, 90, BOB, false));
    assert_policy(&responsive.message(), 90, BOB, true);
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        IDENTITY_LIMIT,
        "the independent listener must not borrow or re-enter a held resolver",
    );
    gate.release();

    for (id, subject, socket) in held {
        assert_policy(&Reply::sent(socket).message(), id, subject, true);
    }
    assert_eq!(gate.calls.load(Ordering::SeqCst), IDENTITY_LIMIT);
    let mut fresh = Reply::sent(modern_socket(slow, 91, BOB, false));
    assert_policy(&fresh.message(), 91, BOB, true);
    exercise_legacy_owner(slow, Reply::sent(legacy));
}

#[test]
fn held_native_identity_admission_keeps_other_listeners_live_and_preserves_custody() {
    assert!(install(backend()), "one backend per integration binary");
    let (entered, arrivals) = mpsc::channel();
    let gate = Arc::new(Gate {
        io_thread: thread::current().id(),
        calls: AtomicUsize::new(0),
        entered,
        open: Mutex::new(false),
        changed: Condvar::new(),
    });
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("identity admission reactor"))
        .build()
        .expect("identity admission runtime");
    runtime.block_on(async move {
        let cx = Cx::current().expect("identity admission runtime context");
        let held_gate = Arc::clone(&gate);
        let slow_identity = Identity::behind_serve(Client::LoopbackHttp);
        let slow = fsonos_mcp::server_answering_as(Vec::new(), move |peer, headers| {
            assert!(peer.ip().is_loopback());
            held_gate.calls.fetch_add(1, Ordering::SeqCst);
            if headers
                .iter()
                .any(|(name, value)| name == HOLD && value == "yes")
            {
                held_gate.wait();
            }
            slow_identity.of_http_parts(peer, headers)
        });
        let fast_identity = Identity::behind_serve(Client::LoopbackHttp);
        let independent = fsonos_mcp::server_answering_as(Vec::new(), move |peer, headers| {
            fast_identity.of_http_parts(peer, headers)
        });
        let slow = Box::pin(slow.bind_http(&cx, "127.0.0.1:0"))
            .await
            .expect("bind listener with held identity lookup");
        let independent = Box::pin(independent.bind_http(&cx, "127.0.0.1:0"))
            .await
            .expect("bind independent listener");
        let addresses = [
            slow.local_addr().unwrap(),
            independent.local_addr().unwrap(),
        ];
        let stop = StopServers {
            cx: cx.clone(),
            addresses,
            gate: Arc::clone(&gate),
        };
        let controller = thread::spawn(move || {
            let _stop = stop;
            exercise(addresses[0], addresses[1], &gate, &arrivals);
        });
        // Poll both accept loops on this one thread; a synchronous identity
        // callback on either loop would prevent the independent response.
        let mut slow = Box::pin(slow.serve(&cx));
        let mut independent = Box::pin(independent.serve(&cx));
        let mut slow_done = None;
        let mut independent_done = None;
        let (slow_done, independent_done) = poll_fn(|task| {
            if slow_done.is_none()
                && let Poll::Ready(done) = slow.as_mut().poll(task)
            {
                slow_done = Some(done);
            }
            if independent_done.is_none()
                && let Poll::Ready(done) = independent.as_mut().poll(task)
            {
                independent_done = Some(done);
            }
            if slow_done.is_some() && independent_done.is_some() {
                Poll::Ready((slow_done.take().unwrap(), independent_done.take().unwrap()))
            } else {
                Poll::Pending
            }
        })
        .await;
        controller
            .join()
            .expect("native identity admission assertions");
        for shutdown in [slow_done, independent_done] {
            if let fastmcp_server::HttpServerShutdown::Nonquiescent(mut children) =
                shutdown.expect("serve native HTTP listener")
            {
                assert_eq!(
                    children.settle_for(Duration::from_secs(2)).await,
                    fastmcp_server::HttpShutdownSettlement::Settled,
                    "the test must leave no HTTP children running",
                );
            }
        }
    });
}
