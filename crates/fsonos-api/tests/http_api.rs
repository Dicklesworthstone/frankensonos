//! The HTTP API over a real loopback socket: zones, control, the house
//! policy, and the coded failures, against a canned speaker transport.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, Room};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};
use serde_json::{Value, json};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

/// Answers every SOAP action with success and `out_args`; records actions.
struct Canned {
    out_args: &'static str,
    sent: Arc<Mutex<Vec<String>>>,
}

impl Transport for Canned {
    fn soap_post(&self, _: IpAddr, _: &str, action: &str, _: &str) -> Result<String, ProtoError> {
        let action = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or_default()
            .to_string();
        self.sent.lock().unwrap().push(action.clone());
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
            self.out_args
        ))
    }
}

/// One S2 household: `Living Room` coordinating `Kitchen`.
fn house() -> Vec<HouseholdState> {
    let id = |s: &str| PlayerId(s.into());
    let player = |pid: &str, room: &str| Player {
        id: id(pid),
        room_name: room.into(),
        ip: "192.0.2.10".parse().unwrap(),
        model: String::new(),
        generation: Generation::S2,
    };
    let room = |name: &str, pid: &str| Room {
        name: name.into(),
        primary: id(pid),
        players: vec![id(pid)],
        missing: Vec::new(),
        coordinator: id("RINCON_LIV"),
    };
    vec![HouseholdState {
        players: vec![
            player("RINCON_LIV", "Living Room"),
            player("RINCON_KIT", "Kitchen"),
        ],
        groups: vec![ZoneGroup {
            coordinator: id("RINCON_LIV"),
            members: vec![id("RINCON_LIV"), id("RINCON_KIT")],
        }],
        rooms: vec![
            room("Living Room", "RINCON_LIV"),
            room("Kitchen", "RINCON_KIT"),
        ],
        ..Default::default()
    }]
}

/// The API over the canned transport and [`house`], answering every caller
/// as `client`.
fn app(
    out_args: &'static str,
    sent: &Arc<Mutex<Vec<String>>>,
    client: &Client,
    web: &WebPolicy,
) -> fastapi::App {
    app_with(
        out_args,
        sent,
        Policy::default(),
        &fsonos_api::Identity::fixed(client.clone()),
        web,
    )
}

/// [`app`] under `policy`, identifying callers by `identity`.
fn app_with(
    out_args: &'static str,
    sent: &Arc<Mutex<Vec<String>>>,
    policy: Policy,
    identity: &fsonos_api::Identity,
    web: &WebPolicy,
) -> fastapi::App {
    let surface = Surface::new(
        Box::new(Canned {
            out_args,
            sent: Arc::clone(sent),
        }),
        Box::new(|_| Ok(house())),
        policy,
        Box::new(SystemClock),
    );
    fsonos_api::app(&Arc::new(surface), identity, web)
}

struct Api {
    addr: SocketAddr,
    server: Arc<TcpServer>,
    thread: Option<JoinHandle<()>>,
    sent: Arc<Mutex<Vec<String>>>,
}

impl Api {
    /// The API on an ephemeral loopback port, answering as `client`.
    fn start(out_args: &'static str, client: &Client) -> Self {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = app(out_args, &sent, client, &web);
        Self::serve(app, sent, &web)
    }

    /// `app` on an ephemeral loopback port.
    fn serve(app: fastapi::App, sent: Arc<Mutex<Vec<String>>>, web: &WebPolicy) -> Self {
        let app = Arc::new(app);
        let config = ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec());
        let server = Arc::new(TcpServer::new(config));
        let (addr_tx, addr_rx) = mpsc::channel();
        let thread = {
            let server = Arc::clone(&server);
            thread::spawn(move || {
                runtime().block_on(async move {
                    let cx = Cx::current().expect("ambient Cx");
                    let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("bind loopback");
                    addr_tx
                        .send(listener.local_addr().expect("local addr"))
                        .expect("report addr");
                    let _ = server.serve_on_app(&cx, listener, app).await;
                });
            })
        };
        let addr = addr_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server ready");
        Self {
            addr,
            server,
            thread: Some(thread),
            sent,
        }
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.call(path, None)
    }

    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.call(path, Some(body.to_string()))
    }

    fn call(&self, path: &str, body: Option<String>) -> (u16, Value) {
        let url = format!("http://{}{path}", self.addr);
        let (status, bytes) = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = HttpClient::default_for_runtime(&cx);
            let request = match body {
                Some(body) => http
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(body),
                None => http.get(url),
            };
            let resp = request.send(&cx).await.expect("HTTP round trip");
            (resp.status, resp.body)
        });
        let value = serde_json::from_slice(&bytes).unwrap_or_else(|_| {
            panic!(
                "{path}: non-JSON body {:?}",
                String::from_utf8_lossy(&bytes)
            )
        });
        (status, value)
    }

    /// A raw request with exactly these headers; returns the status code
    /// and the body.
    fn raw(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &str) -> (u16, String) {
        use std::io::{Read, Write};
        let mut stream = std::net::TcpStream::connect(self.addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut request = format!("{method} {path} HTTP/1.1\r\n");
        for (name, value) in headers {
            request.push_str(name);
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
        request.push_str("Connection: close\r\nContent-Length: ");
        request.push_str(&body.len().to_string());
        request.push_str("\r\n\r\n");
        request.push_str(body);
        stream.write_all(request.as_bytes()).expect("send");
        let mut answer = String::new();
        let _ = stream.read_to_string(&mut answer);
        let status = answer
            .split_whitespace()
            .nth(1)
            .and_then(|c| c.parse().ok())
            .unwrap_or(0);
        let body = answer
            .split_once("\r\n\r\n")
            .map_or("", |(_, b)| b)
            .to_string();
        (status, body)
    }

    fn host(&self) -> String {
        self.addr.to_string()
    }

    fn actions(&self) -> Vec<String> {
        self.sent.lock().unwrap().clone()
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[test]
fn zones_list_and_one_zone_by_percent_encoded_name() {
    let api = Api::start(
        "<CurrentTransportState>PLAYING</CurrentTransportState>",
        &Client::LoopbackHttp,
    );
    let (status, zones) = api.get("/zones");
    assert_eq!(status, 200, "{zones}");
    assert_eq!(zones[0]["members"], json!(["Living Room", "Kitchen"]));
    assert_eq!(zones[0]["transport_state"], "playing");

    let (status, zone) = api.get("/zones/living%20room");
    assert_eq!(status, 200, "{zone}");
    assert_eq!(zone["coordinator_room"], "Living Room");

    let (status, err) = api.get("/zones/Kitchn");
    assert_eq!(status, 404, "{err}");
    assert_eq!(err["code"], "UNKNOWN_ROOM");
    assert_eq!(err["suggestions"], json!(["Kitchen@S2"]));
}

#[test]
fn control_routes_address_the_coordinator() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, out) = api.post("/pause", &json!({ "zone": "Kitchen" }));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["done"], "paused Living Room's group");
    assert_eq!(out["changed"], true);
    let (status, _) = api.post("/group", &json!({ "zone": "Kitchen", "to": "Living Room" }));
    assert_eq!(status, 200);
    let (status, out) = api.post("/ungroup", &json!({ "zone": "Kitchen" }));
    assert_eq!(status, 200, "{out}");
    assert_eq!(
        api.actions(),
        ["Pause", "BecomeCoordinatorOfStandaloneGroup"]
    );
}

#[test]
fn agents_over_http_are_volume_capped_with_a_note() {
    let api = Api::start("<CurrentVolume>60</CurrentVolume>", &Client::LoopbackHttp);
    let (status, out) = api.post("/volume", &json!({ "zone": "Kitchen", "volume": 100 }));
    assert_eq!(status, 200, "{out}");
    assert_eq!(out["volume"], 70);
    assert_eq!(out["notes"][0]["code"], "VOLUME_CLAMPED");
    assert_eq!(api.actions(), ["GetVolume", "SetVolume"]);
}

#[test]
fn bad_requests_are_coded() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, err) = api.post("/pause", &json!({ "room": "Kitchen" }));
    assert_eq!(status, 422, "{err}");
    assert_eq!(err["code"], "INVALID_ARGUMENT");
    assert!(
        err["detail"]
            .as_str()
            .unwrap()
            .contains("unknown field `room`")
    );
    let (status, err) = api.post("/volume", &json!({ "zone": "Kitchen", "volume": 101 }));
    assert_eq!(
        (status, err["code"].as_str()),
        (422, Some("INVALID_ARGUMENT"))
    );
    let (status, err) = api.post("/dj/start", &json!({ "zone": "Kitchen" }));
    assert_eq!(
        (status, err["code"].as_str()),
        (501, Some("NOT_IMPLEMENTED"))
    );
    assert_eq!(api.actions(), Vec::<String>::new());
}

#[test]
fn unknown_callers_may_read_but_not_control() {
    let api = Api::start("", &Client::Unknown);
    let (status, _) = api.get("/zones");
    assert_eq!(status, 200);
    let (status, err) = api.post("/pause", &json!({ "zone": "Kitchen" }));
    assert_eq!(status, 403, "{err}");
    assert_eq!(err["code"], "POLICY_DENIED");
    assert_eq!(err["retryable"], false);
    // Reading the zone's state is all an unknown caller caused.
    assert_eq!(api.actions(), ["GetTransportInfo"]);
}

#[test]
fn a_foreign_host_is_refused_dns_rebinding() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, _) = api.raw("GET", "/zones", &[("Host", "evil.example")], "");
    assert_eq!(status, 400, "a Host the listener does not own is refused");
    let (status, _) = api.raw(
        "GET",
        "/health",
        &[("Host", &format!("localhost:{}", api.addr.port()))],
        "",
    );
    assert_eq!(status, 200);
    assert_eq!(api.actions(), Vec::<String>::new());
}

#[test]
fn writes_must_be_json_so_no_preflight_posts_fail() {
    let api = Api::start("", &Client::LoopbackHttp);
    let host = api.host();
    let (status, body) = api.raw(
        "POST",
        "/pause",
        &[("Host", &host), ("Content-Type", "text/plain")],
        r#"{"zone":"Kitchen"}"#,
    );
    assert_eq!(status, 415, "{body}");
    assert!(body.contains("UNSUPPORTED_MEDIA_TYPE"), "{body}");
    let (status, _) = api.raw("POST", "/undo", &[("Host", &host)], "");
    assert_eq!(status, 415, "even an empty write needs the JSON type");
    assert_eq!(api.actions(), Vec::<String>::new());
}

#[test]
fn only_the_daemons_own_origins_may_call() {
    let api = Api::start("", &Client::LoopbackHttp);
    let host = api.host();
    let (status, body) = api.raw(
        "GET",
        "/zones",
        &[("Host", &host), ("Origin", "https://evil.example")],
        "",
    );
    assert_eq!(status, 403, "{body}");
    assert!(
        body.contains("UNTRUSTED_ORIGIN") && !body.contains("Access-Control"),
        "{body}"
    );
    let (status, body) = api.raw(
        "POST",
        "/pause",
        &[
            ("Host", &host),
            ("Origin", "https://localhost"),
            ("Content-Type", "application/json"),
        ],
        r#"{"zone":"Kitchen"}"#,
    );
    assert_eq!(status, 200, "the daemon's own origin may call: {body}");
    assert_eq!(api.actions(), ["Pause"]);
}

#[test]
fn the_openapi_document_describes_every_route() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200, "{doc}");
    assert_eq!(doc["info"]["title"], "FrankenSonos");

    // Every route the app serves is documented, and named.
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let served = app("", &Arc::default(), &Client::LoopbackHttp, &web);
    let mut documented = 0;
    for (method, path) in served.routes().filter(|(_, p)| *p != "/openapi.json") {
        let op = &doc["paths"][path][method.as_str().to_lowercase()];
        assert!(op["operationId"].is_string(), "{method:?} {path}: {op}");
        assert!(op["responses"]["200"].is_object(), "{path}: no 200 answer");
        assert!(
            op["responses"]["404"].is_object(),
            "{path}: no error answers"
        );
        documented += 1;
    }
    assert!(documented >= 20, "only {documented} routes documented");

    // Operation ids are the MCP tools' names.
    assert_eq!(doc["paths"]["/volume"]["post"]["operationId"], "set_volume");
    assert_eq!(doc["paths"]["/undo"]["post"]["operationId"], "undo_last");
    assert_eq!(
        doc["paths"]["/zones/{room}/state"]["get"]["operationId"],
        "get_zone_state"
    );

    // Bodies have their schemas, with the required fields.
    let body = &doc["paths"]["/volume"]["post"]["requestBody"];
    assert_eq!(body["required"], true);
    let schema = &body["content"]["application/json"]["schema"];
    let volume = schema["$ref"]
        .as_str()
        .and_then(|r| r.rsplit('/').next())
        .map_or(schema, |name| &doc["components"]["schemas"][name]);
    for field in ["zone", "volume", "delta", "group"] {
        assert!(volume["properties"][field].is_object(), "{field}: {volume}");
    }
    assert_eq!(volume["required"], json!(["zone"]), "{volume}");

    // Error answers carry the wire codes.
    let not_found = doc["paths"]["/zones/{room}"]["get"]["responses"]["404"]["description"]
        .as_str()
        .unwrap();
    assert!(not_found.contains("UNKNOWN_ROOM"), "{not_found}");
    assert!(
        doc["paths"]["/pause"]["post"]["responses"]["415"].is_object(),
        "writes document 415"
    );
    assert!(
        doc["paths"]["/zones"]["get"]["responses"]["415"].is_null(),
        "reads are never refused for their media type"
    );
    let text = doc.to_string();
    assert!(
        text.contains("\"INVALID_ARGUMENT\""),
        "error codes by wire name"
    );
    assert!(!text.contains("\"InvalidArgument\""), "not by variant name");

    // The document is served behind the same browser rules as the rest.
    let (status, _) = api.raw(
        "GET",
        "/openapi.json",
        &[("Host", &api.host()), ("Origin", "https://evil.example")],
        "",
    );
    assert_eq!(status, 403);
}

#[test]
fn the_event_stream_needs_the_live_model() {
    // This surface surveys on demand; only `fsonos serve`'s has events.
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, err) = api.get("/events");
    assert_eq!(status, 501, "{err}");
    assert_eq!(err["code"], "NOT_IMPLEMENTED");
    assert!(err["hint"].as_str().unwrap().contains("fsonos serve"));
    let (status, err) = api.get("/events?since=soon");
    assert_eq!(
        (status, err["code"].as_str()),
        (501, Some("NOT_IMPLEMENTED"))
    );
}

#[test]
fn search_and_history_validate_and_answer_empty_without_a_store() {
    let api = Api::start("", &Client::LoopbackHttp);
    let (status, err) = api.get("/library/search");
    assert_eq!(
        (status, err["code"].as_str()),
        (422, Some("INVALID_ARGUMENT"))
    );
    let (status, hits) = api.get("/library/search?q=goldberg+gould");
    assert_eq!((status, hits), (200, json!([])));
    let (status, err) = api.get("/library/search?q=x&limit=500");
    assert_eq!(
        (status, err["code"].as_str()),
        (422, Some("INVALID_ARGUMENT"))
    );
    let (status, plays) = api.get("/history");
    assert_eq!((status, plays), (200, json!([])));
    let (status, err) = api.get("/history?limit=0");
    assert_eq!(
        (status, err["code"].as_str()),
        (422, Some("INVALID_ARGUMENT"))
    );
}

#[test]
fn behind_serve_the_login_header_is_the_caller() {
    // Loopback callers may not pause; the tailnet user Ada may.
    let policy = Policy::from_toml(
        "[clients.\"loopback-http\"]\ndeny = [\"pause\"]\n\n[clients.\"ada@example.com\"]\nallow = [\"pause\"]\n",
    )
    .unwrap();
    let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
    let sent = Arc::new(Mutex::new(Vec::new()));
    let identity = fsonos_api::Identity::behind_serve(Client::LoopbackHttp);
    let api = Api::serve(
        app_with("", &sent, policy, &identity, &web),
        Arc::clone(&sent),
        &web,
    );
    let host = api.host();
    let pause = |login: Option<&str>| {
        let mut headers = vec![
            ("Host", host.as_str()),
            ("Content-Type", "application/json"),
        ];
        if let Some(login) = login {
            headers.push(("Tailscale-User-Login", login));
        }
        api.raw("POST", "/pause", &headers, r#"{"zone":"Kitchen"}"#)
            .0
    };
    assert_eq!(pause(None), 403, "loopback-http is denied pause");
    assert_eq!(
        pause(Some("ada@example.com")),
        200,
        "Ada, via Serve, may pause"
    );
    assert_eq!(
        pause(Some("eve@example.com")),
        200,
        "an unlisted login gets the defaults"
    );
    assert_eq!(api.actions(), ["Pause", "Pause"]);
}
