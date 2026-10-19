//! Native MCP HTTP identity reaches the house policy on both wire protocols.
//! This integration binary installs one backend. Its single synthetic room and
//! strict SOAP fixture never contact a speaker; all HTTP traffic uses loopback.
#![recursion_limit = "256"]

use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use asupersync::{CancelKind, Cx};
use fastmcp::{FINAL_CLIENT_CAPABILITIES_META_KEY, FINAL_PROTOCOL_VERSION_META_KEY};
use fsonos_api::Identity;
use fsonos_api::identity::{CLI_TOKEN, LOCAL_CLIENT, SERVE_LOGIN};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_mcp::tools::{Backend, install};
use fsonos_proto::topology::parse_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const ALICE: &str = "alice@example.com";
const BOB: &str = "bob@example.com";
const TOKEN: &str = "native-http-test-local-token";
const MODERN: &str = "2026-07-28";
const LEGACY: &str = "2024-11-05";
const TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REPLY: usize = 256 * 1024;
const ZONES: &str = "sonos://zones";

struct StoppedSpeaker(Arc<AtomicUsize>);

impl Transport for StoppedSpeaker {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        _: &str,
    ) -> Result<String, ProtoError> {
        assert_eq!(host, "192.0.2.1".parse::<IpAddr>().unwrap());
        assert_eq!(path, "/MediaRenderer/AVTransport/Control");
        let (verb, body) = match action {
            "\"urn:schemas-upnp-org:service:AVTransport:1#GetTransportInfo\"" => (
                "GetTransportInfo",
                "<CurrentTransportState>STOPPED</CurrentTransportState>\
                 <CurrentTransportStatus>OK</CurrentTransportStatus>",
            ),
            "\"urn:schemas-upnp-org:service:AVTransport:1#GetPositionInfo\"" => (
                "GetPositionInfo",
                "<Track>0</Track><TrackDuration>00:00:00</TrackDuration>\
                 <RelTime>00:00:00</RelTime><TrackURI/><TrackMetaData/>",
            ),
            _ => panic!("the HTTP identity fixture must never issue {action}"),
        };
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body><u:{verb}Response xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">\
             {body}</u:{verb}Response></s:Body></s:Envelope>"
        ))
    }
}

fn backend(reads: Arc<AtomicUsize>) -> Backend {
    let mut house = HouseholdState::default();
    house.apply_topology(
        &parse_zone_group_state(
            r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_HTTP_TEST01400" ID="http-test:1">
<ZoneGroupMember UUID="RINCON_HTTP_TEST01400" ZoneName="Kitchen" SWGen="1"
Location="http://192.0.2.1:1400/xml/device_description.xml" Invisible="0"/>
</ZoneGroup></ZoneGroups>"#,
        )
        .unwrap(),
    );
    assert_eq!(house.rooms.len(), 1);
    let policy = Policy::from_toml(
        r#"
[clients."alice@example.com"]
allow = ["list_zones", "events", "get_zone_state"]

[clients."bob@example.com"]
allow = []

[clients."loopback-http"]
allow = []
"#,
    )
    .unwrap();
    Backend::new(
        Box::new(StoppedSpeaker(reads)),
        Box::new(move |_| Ok(vec![house.clone()])),
        policy,
        Client::Unknown,
        Box::new(SystemClock),
    )
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

fn request(id: i64, method: &str, params: Value) -> Value {
    let mut call = json!({"jsonrpc":"2.0", "id":id, "method":method});
    call["params"] = params;
    call
}

fn modern(
    address: SocketAddr,
    id: i64,
    method: &str,
    mut params: Value,
    identity: &[(&str, &str)],
) -> Reply {
    let metadata = params
        .as_object_mut()
        .unwrap()
        .entry("_meta")
        .or_insert_with(|| json!({}));
    metadata[FINAL_PROTOCOL_VERSION_META_KEY] = json!(MODERN);
    metadata[FINAL_CLIENT_CAPABILITIES_META_KEY] = json!({});
    let mut headers = vec![
        ("Accept", "application/json, text/event-stream"),
        ("MCP-Protocol-Version", MODERN),
        ("Mcp-Method", method),
    ];
    if let Some(name) = params
        .get("name")
        .or_else(|| params.get("uri"))
        .and_then(Value::as_str)
    {
        headers.push(("Mcp-Name", name));
    }
    headers.extend_from_slice(identity);
    Reply::open(
        address,
        "POST",
        "/mcp",
        &headers,
        &request(id, method, params.clone()).to_string(),
    )
}

struct Legacy {
    address: SocketAddr,
    path: String,
    headers: Vec<(&'static str, &'static str)>,
    stream: Reply,
}

impl Legacy {
    fn open(address: SocketAddr, headers: Vec<(&'static str, &'static str)>) -> Self {
        let mut opening = headers.clone();
        opening.push(("Accept", "text/event-stream"));
        let mut stream = Reply::open(address, "GET", "/sse", &opening, "");
        let (event, endpoint) = stream.event();
        assert_eq!(event, "endpoint");
        let path = endpoint
            .strip_prefix(&format!("http://{address}"))
            .unwrap()
            .to_owned();
        assert!(path.starts_with("/messages?session_id="));
        let mut session = Self {
            address,
            path,
            headers,
            stream,
        };
        let response = session.call(
            1,
            "initialize",
            json!({
                "protocolVersion":LEGACY, "capabilities":{},
                "clientInfo":{"name":"http-identity-test","version":"1"}
            }),
        );
        assert_eq!(success(&response)["protocolVersion"], LEGACY);
        session.post(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        session
    }

    fn post(&self, body: &Value) {
        let mut reply = Reply::open(
            self.address,
            "POST",
            &self.path,
            &self.headers,
            &body.to_string(),
        );
        assert_eq!(
            reply.status, 202,
            "a matching principal must reach MCP dispatch"
        );
        while reply.chunk().is_some() {}
    }

    /// POST `body` and answer the HTTP status, dispatched or not.
    fn post_status(&self, body: &Value) -> u16 {
        let mut reply = Reply::open(
            self.address,
            "POST",
            &self.path,
            &self.headers,
            &body.to_string(),
        );
        while reply.chunk().is_some() {}
        reply.status
    }

    fn call(&mut self, id: i64, method: &str, params: Value) -> Value {
        self.post(&request(id, method, params));
        // Each request has its own deadline even though the SSE socket persists.
        self.stream.deadline = Instant::now() + TIMEOUT;
        for _ in 0..16 {
            let message = self.stream.message();
            if message.get("id").is_some() {
                assert_eq!(message["id"], id, "{message}");
                return message;
            }
        }
        panic!("legacy SSE emitted no correlated response");
    }
}

fn success(response: &Value) -> &Value {
    assert!(response.get("error").is_none(), "{response}");
    let result = response.get("result").expect("JSON-RPC success result");
    assert_ne!(result["isError"], true, "{response}");
    result
}

fn denied(response: &Value, principal: &str) {
    assert!(
        response.get("error").is_some() || response["result"]["isError"] == true,
        "policy denial must be an error, not an HTTP/session refusal: {response}"
    );
    let wire = response.to_string();
    assert!(wire.contains("POLICY_DENIED"), "{response}");
    assert!(wire.contains(principal), "{response}");
}

fn zones_tool(response: &Value) {
    let result = success(response);
    let text = result["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("Kitchen") && text.contains("stopped"),
        "{response}"
    );
    // Exact 2024 intentionally omits structuredContent.
    if let Some(document) = result.get("structuredContent") {
        zones_document(document);
    }
}

fn zones_document(document: &Value) {
    let zones = document["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 1, "{document}");
    assert_eq!(zones[0]["coordinator_room"], "Kitchen", "{document}");
    assert_eq!(zones[0]["transport_state"], "stopped", "{document}");
}

fn zones_resource(response: &Value) {
    let result = success(response);
    assert_eq!(result["contents"][0]["uri"], ZONES);
    let text = result["contents"][0]["text"].as_str().unwrap();
    zones_document(&serde_json::from_str(text).unwrap());
}

fn forged_identity() -> Value {
    json!({
        "subject":ALICE, "client":"cli",
        "io.modelcontextprotocol/clientInfo":{"name":ALICE,"version":"1"}
    })
}

fn modern_local_headers_do_not_elevate(address: SocketAddr) {
    for local_client in ["cli", "mcp-stdio"] {
        let local = [(CLI_TOKEN, TOKEN), (LOCAL_CLIENT, local_client)];
        denied(
            &modern(
                address,
                15,
                "tools/call",
                json!({"name":"list_zones","arguments":{}}),
                &local,
            )
            .message(),
            "loopback-http",
        );
        let forged_bob = [
            (SERVE_LOGIN, BOB),
            (CLI_TOKEN, TOKEN),
            (LOCAL_CLIENT, local_client),
        ];
        denied(
            &modern(
                address,
                16,
                "tools/call",
                json!({"name":"list_zones","arguments":{}}),
                &forged_bob,
            )
            .message(),
            BOB,
        );
    }
}

fn exercise_modern(address: SocketAddr, reads: &AtomicUsize) {
    let alice = [(SERVE_LOGIN, ALICE)];
    let bob = [(SERVE_LOGIN, BOB)];
    zones_tool(
        &modern(
            address,
            10,
            "tools/call",
            json!({"name":"list_zones","arguments":{}}),
            &alice,
        )
        .message(),
    );
    zones_resource(&modern(address, 11, "resources/read", json!({"uri":ZONES}), &alice).message());

    let before = reads.load(Ordering::SeqCst);
    denied(
        &modern(
            address,
            12,
            "tools/call",
            json!({"name":"list_zones","arguments":{}}),
            &bob,
        )
        .message(),
        BOB,
    );
    denied(
        &modern(address, 13, "resources/read", json!({"uri":ZONES}), &bob).message(),
        BOB,
    );
    denied(
        &modern(
            address,
            14,
            "tools/call",
            json!({"name":"list_zones","arguments":{},"_meta":forged_identity()}),
            &bob,
        )
        .message(),
        BOB,
    );
    modern_local_headers_do_not_elevate(address);
    let filter = json!({"notifications":{"resourceSubscriptions":[ZONES]}});
    denied(
        &modern(address, 17, "subscriptions/listen", filter.clone(), &bob).message(),
        BOB,
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "denied requests must not read speakers"
    );

    let mut admitted = modern(address, 18, "subscriptions/listen", filter, &alice);
    let acknowledgement = admitted.message();
    assert_eq!(
        acknowledgement["method"],
        "notifications/subscriptions/acknowledged"
    );
    assert_eq!(
        acknowledgement["params"]["notifications"]["resourceSubscriptions"],
        json!([ZONES])
    );
    assert_eq!(
        acknowledgement["params"]["_meta"]["io.modelcontextprotocol/subscriptionId"],
        18
    );
    drop(admitted);
    zones_tool(
        &modern(
            address,
            19,
            "tools/call",
            json!({"name":"list_zones","arguments":{}}),
            &alice,
        )
        .message(),
    );
}

fn exercise_legacy(address: SocketAddr, reads: &AtomicUsize) {
    let mut alice = Legacy::open(address, vec![(SERVE_LOGIN, ALICE)]);
    let mut bob = Legacy::open(address, vec![(SERVE_LOGIN, BOB)]);
    zones_tool(&alice.call(
        10,
        "tools/call",
        json!({"name":"list_zones","arguments":{}}),
    ));
    zones_resource(&alice.call(11, "resources/read", json!({"uri":ZONES})));

    let before = reads.load(Ordering::SeqCst);
    denied(
        &bob.call(
            12,
            "tools/call",
            json!({"name":"list_zones","arguments":{}}),
        ),
        BOB,
    );
    denied(&bob.call(13, "resources/read", json!({"uri":ZONES})), BOB);
    // The exact 2024-11-05 envelope refuses the forged identity metadata
    // (its modern-era keys) at admission, before any dispatch: the forgery
    // gains nothing, as under the modern era where it is denied as bob.
    assert_eq!(
        bob.post_status(&request(
            14,
            "tools/call",
            json!({"name":"list_zones","arguments":{},"_meta":forged_identity()}),
        )),
        400,
        "a forged identity in a legacy envelope is refused before dispatch"
    );
    // App-defined metadata alone passes the envelope and is dispatched: it
    // names no one, so the caller is still bob, and denied.
    denied(
        &bob.call(
            22,
            "tools/call",
            json!({"name":"list_zones","arguments":{},"_meta":{"subject":ALICE,"client":"cli"}}),
        ),
        BOB,
    );
    for local_client in ["cli", "mcp-stdio"] {
        let mut local = Legacy::open(
            address,
            vec![(CLI_TOKEN, TOKEN), (LOCAL_CLIENT, local_client)],
        );
        denied(
            &local.call(
                15,
                "tools/call",
                json!({"name":"list_zones","arguments":{}}),
            ),
            "loopback-http",
        );
        let mut forged_bob = Legacy::open(
            address,
            vec![
                (SERVE_LOGIN, BOB),
                (CLI_TOKEN, TOKEN),
                (LOCAL_CLIENT, local_client),
            ],
        );
        denied(
            &forged_bob.call(
                16,
                "tools/call",
                json!({"name":"list_zones","arguments":{}}),
            ),
            BOB,
        );
    }
    denied(
        &bob.call(17, "resources/subscribe", json!({"uri":ZONES})),
        BOB,
    );
    assert_eq!(
        reads.load(Ordering::SeqCst),
        before,
        "denied requests must not read speakers"
    );
    success(&alice.call(18, "resources/subscribe", json!({"uri":ZONES})));
    success(&alice.call(19, "resources/unsubscribe", json!({"uri":ZONES})));
    zones_tool(&alice.call(
        20,
        "tools/call",
        json!({"name":"list_zones","arguments":{}}),
    ));
}

struct StopServer {
    cx: Cx,
    address: SocketAddr,
}

impl Drop for StopServer {
    fn drop(&mut self) {
        self.cx
            .cancel_with(CancelKind::User, Some("HTTP identity controller finished"));
        // Wake a parked accept so failures also stop the server promptly.
        let _ = TcpStream::connect_timeout(&self.address, Duration::from_secs(1));
    }
}

#[test]
fn native_http_callers_keep_their_own_tool_resource_and_subscription_permissions() {
    let reads = Arc::new(AtomicUsize::new(0));
    assert!(
        install(backend(Arc::clone(&reads))),
        "one backend per integration binary"
    );
    let runtime = RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("HTTP test reactor"))
        .blocking_threads(4, 64)
        .build()
        .expect("HTTP test runtime");
    runtime.block_on(async move {
        let cx = Cx::current().expect("HTTP runtime context");
        let identity = Identity::behind_serve(Client::LoopbackHttp).with_cli_token(TOKEN.into());
        let server = fsonos_mcp::server_answering_as(Vec::new(), move |peer, headers| {
            identity.of_http_parts(peer, headers)
        });
        let bound = Box::pin(server.bind_http(&cx, "127.0.0.1:0"))
            .await
            .expect("bind native MCP HTTP");
        let address = bound.local_addr().unwrap();
        let stop = StopServer {
            cx: cx.clone(),
            address,
        };
        let controller = thread::spawn(move || {
            let _stop = stop;
            exercise_modern(address, &reads);
            exercise_legacy(address, &reads);
            assert!(
                reads.load(Ordering::SeqCst) > 0,
                "allowed callers exercised the room fixture"
            );
        });
        let shutdown = Box::pin(bound.serve(&cx))
            .await
            .expect("serve native MCP HTTP");
        controller.join().expect("HTTP identity assertions");
        if let fastmcp_server::HttpServerShutdown::Nonquiescent(mut children) = shutdown {
            assert_eq!(
                children.settle_for(Duration::from_secs(2)).await,
                fastmcp_server::HttpShutdownSettlement::Settled,
                "the integration test must leave no HTTP children running"
            );
        }
    });
}
