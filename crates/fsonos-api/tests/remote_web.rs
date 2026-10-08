//! The web remote over real loopback sockets, against `fsonos-sim`:
//!
//! * `GET /` and its assets come with headers that keep the page to itself;
//! * every route the page calls exists (its ROUTES table against the
//!   OpenAPI document);
//! * a control POST from the page behind Tailscale Serve is admitted from
//!   exactly the learned Serve origin, and refused (403 UNTRUSTED_ORIGIN)
//!   from any other ts.net page, before the origin is learned and after it
//!   is withdrawn;
//! * `GET /art` serves the image the zone's own player reports, fetched
//!   from that player only: a caller-supplied URL is refused and nothing
//!   is fetched, art elsewhere is not fetched (204), and another site's
//!   page gets none.

use asupersync::Cx;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::web::ServeOrigin;
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::soap::{self, AV_TRANSPORT, args_xml};
use fsonos_proto::ssdp::Advert;
use fsonos_proto::{HttpBody, ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimTransport};
use serde_json::{Value, json};
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

/// A placeholder MagicDNS name.
const NAME: &str = "sonos-host.example-tailnet.ts.net";

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

/// The sim's LAN, recording every byte fetch (album art) it is asked for.
struct Recording {
    lan: SimLan,
    fetched: Arc<Mutex<Vec<String>>>,
}

impl Transport for Recording {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.lan.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.fetched.lock().unwrap().push(url.to_owned());
        self.lan.http_get_bytes(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        self.lan.ssdp_search(mx_secs, wait)
    }
}

/// The API on a loopback port, as `fsonos serve` runs it: the browser rules
/// for the bound address, plus what `serve` learns.
struct Remote {
    addr: SocketAddr,
    server: Arc<TcpServer>,
    fetched: Arc<Mutex<Vec<String>>>,
    serve: ServeOrigin,
}

impl Remote {
    fn start(sim: &SimHandle) -> Self {
        let fetched = Arc::new(Mutex::new(Vec::new()));
        let surface = Arc::new(Surface::new(
            Box::new(Recording {
                lan: sim.lan(),
                fetched: Arc::clone(&fetched),
            }),
            Box::new(|t| {
                Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
            }),
            Policy::default(),
            Box::new(SystemClock),
        ));
        let serve = ServeOrigin::default();
        let hosts = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[])
            .hosts()
            .to_vec();
        let server = Arc::new(TcpServer::new(
            ServerConfig::new("127.0.0.1:0").with_allowed_hosts(hosts),
        ));
        let (addr_tx, addr_rx) = mpsc::channel();
        {
            let (server, serve) = (Arc::clone(&server), serve.clone());
            thread::spawn(move || {
                runtime().block_on(async move {
                    let cx = Cx::current().expect("ambient Cx");
                    let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .expect("bind loopback");
                    let local = listener.local_addr().expect("local addr");
                    let web = WebPolicy::for_listener(local, &[]).with_serve_origin(&serve);
                    let identity = Identity::fixed(Client::LoopbackHttp);
                    let app = Arc::new(fsonos_api::app(&surface, &identity, &web));
                    addr_tx.send(local).expect("report addr");
                    let _ = server.serve_on_app_concurrent(&cx, listener, app).await;
                });
            });
        }
        let addr = addr_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("server ready");
        Self {
            addr,
            server,
            fetched,
            serve,
        }
    }

    fn fetched(&self) -> Vec<String> {
        std::mem::take(&mut *self.fetched.lock().unwrap())
    }
}

impl Drop for Remote {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(TcpStream::connect_timeout(
            &self.addr,
            Duration::from_millis(250),
        ));
    }
}

/// One answer: status, headers (names lowercase) and body.
struct Answer {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Answer {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }
}

/// `method path` with exactly `headers` (Host defaults to the listener's),
/// over a fresh connection.
fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> Answer {
    let mut stream = TcpStream::connect(addr).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("timeout");
    let mut head = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        let _ = write!(head, "Host: {addr}\r\n");
    }
    for (name, value) in headers {
        let _ = write!(head, "{name}: {value}\r\n");
    }
    let _ = write!(head, "Content-Length: {}\r\n\r\n{body}", body.len());
    stream.write_all(head.as_bytes()).expect("send");
    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("a complete head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .expect("a status");
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = raw[split + 4..].to_vec();
    let chunked = headers
        .iter()
        .any(|(k, v)| k == "transfer-encoding" && v.eq_ignore_ascii_case("chunked"));
    if chunked {
        body = dechunk(&body);
    }
    Answer {
        status,
        headers,
        body,
    }
}

fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(eol) = raw.windows(2).position(|w| w == b"\r\n") {
        let size =
            usize::from_str_radix(String::from_utf8_lossy(&raw[..eol]).trim(), 16).unwrap_or(0);
        if size == 0 {
            break;
        }
        out.extend_from_slice(&raw[eol + 2..eol + 2 + size]);
        raw = &raw[eol + 4 + size..];
    }
    out
}

fn get(remote: &Remote, path: &str, headers: &[(&str, &str)]) -> Answer {
    send(remote.addr, "GET", path, headers, "")
}

#[test]
fn the_page_is_served_and_kept_to_itself() {
    let sim = SimHousehold::standard().spawn().expect("sim");
    let remote = Remote::start(&sim);
    let page = get(&remote, "/", &[]);
    assert_eq!(page.status, 200, "{}", page.text());
    assert_eq!(
        page.header("content-type"),
        Some("text/html; charset=utf-8")
    );
    let csp = page.header("content-security-policy").unwrap_or_default();
    for directive in [
        "default-src 'none'",
        "script-src 'self'",
        "connect-src 'self'",
        "frame-ancestors 'none'",
    ] {
        assert!(csp.contains(directive), "{csp}");
    }
    assert_eq!(page.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(
        page.header("cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert!(
        page.text()
            .contains(r#"<script src="/remote.js" defer></script>"#)
    );
    for (path, kind) in [
        ("/remote.js", "text/javascript"),
        ("/remote.css", "text/css"),
    ] {
        let asset = get(&remote, path, &[]);
        assert_eq!(asset.status, 200, "{path}");
        assert!(
            asset
                .header("content-type")
                .unwrap_or_default()
                .starts_with(kind),
            "{path}"
        );
    }
}

/// The page's ROUTES table: `name: ['METHOD', '/path'],` lines.
fn page_routes(script: &str) -> Vec<(String, String)> {
    let start = script.find("const ROUTES = {").expect("a ROUTES table");
    let end = start + script[start..].find("};").expect("its end");
    script[start..end]
        .lines()
        .filter_map(|line| {
            let (_, rest) = line.split_once("['")?;
            let (method, rest) = rest.split_once("', '")?;
            let (path, _) = rest.split_once("']")?;
            Some((method.to_owned(), path.to_owned()))
        })
        .collect()
}

#[test]
fn every_route_the_page_calls_exists() {
    let sim = SimHousehold::standard().spawn().expect("sim");
    let remote = Remote::start(&sim);
    let script = get(&remote, "/remote.js", &[]).text();
    let openapi = get(&remote, "/openapi.json", &[]).json();
    let routes = page_routes(&script);
    assert!(routes.len() >= 10, "{routes:?}");
    for (method, path) in &routes {
        let documented = &openapi["paths"][path][method.to_ascii_lowercase()];
        assert!(
            documented.is_object(),
            "the page calls {method} {path}, which the API lacks"
        );
    }
    // Every request goes through the table: one fetch, one event stream.
    assert_eq!(script.matches("fetch(").count(), 1);
    assert_eq!(script.matches("new EventSource(").count(), 1);
    assert!(script.contains("new EventSource(path('events'))"));
    let table_end = script.find("};").unwrap();
    assert!(
        !script[table_end..].contains("'/") && !script[table_end..].contains("`/"),
        "a path outside ROUTES"
    );
}

#[test]
fn the_serve_origin_is_exactly_the_one_learned() {
    let sim = SimHousehold::standard().spawn().expect("sim");
    let remote = Remote::start(&sim);
    let body = json!({ "zone": "Kitchen", "volume": 20 }).to_string();
    let post = |origin: &str| {
        send(
            remote.addr,
            "POST",
            "/volume",
            &[
                // As Serve forwards it: the client's Host.
                ("Host", NAME),
                ("Origin", origin),
                ("Content-Type", "application/json"),
            ],
            &body,
        )
    };
    let own = format!("https://{NAME}");
    let refused = |answer: &Answer| answer.status == 403 && answer.code() == "UNTRUSTED_ORIGIN";

    // Not learned yet: the page behind Serve may not control.
    assert!(refused(&post(&own)));
    remote.serve.set(Some(NAME)).unwrap();
    let admitted = post(&own);
    assert_eq!(admitted.status, 200, "{}", admitted.text());
    for foreign in [
        "https://attacker.other-tailnet.ts.net",
        "https://sonos-host.other-tailnet.ts.net",
        "https://sonos-host.example-tailnet.ts.net:8443",
        "http://sonos-host.example-tailnet.ts.net",
        "https://sonos-host.example-tailnet.ts.net.evil.example",
        "null",
    ] {
        let answer = post(foreign);
        assert!(
            refused(&answer),
            "{foreign}: {} {}",
            answer.status,
            answer.text()
        );
    }
    // The listener's own loopback origin, on the port it really bound.
    let loopback = format!("http://{}", remote.addr);
    let local = send(
        remote.addr,
        "POST",
        "/volume",
        &[
            ("Origin", loopback.as_str()),
            ("Content-Type", "application/json"),
        ],
        &body,
    );
    assert_eq!(local.status, 200, "{}", local.text());
    // Withdrawn (Serve torn down, or Funnel turned on): refused again.
    remote.serve.set(None).unwrap();
    assert!(refused(&post(&own)));
}

fn avt(t: &SimTransport, action: &str, args: &[(&str, &str)]) {
    let mut all = vec![("InstanceID", "0")];
    all.extend_from_slice(args);
    soap::call(t, t.ip(), &AV_TRANSPORT, action, &args_xml(&all)).expect(action);
}

const DIDL_OPEN: &str = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
     xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
     xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" \
     xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">";

/// Kitchen plays a Spotify track from its queue: its player reports art
/// on itself, as real players do.
fn kitchen_plays_spotify(sim: &SimHandle) {
    let t = sim.transport("Kitchen").unwrap();
    let params = sim.render_params(1).unwrap();
    let uuid = sim.player("Kitchen").unwrap().uuid.clone();
    let uri = "spotify%3atrack%3aRemoteArt";
    let meta = format!(
        "{DIDL_OPEN}<item id=\"{}{uri}\" parentID=\"-1\" restricted=\"true\">\
         <dc:title>Track</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class>\
         <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</desc>\
         </item></DIDL-Lite>",
        params.item_id_prefix, params.cdudn
    );
    avt(
        &t,
        "AddURIToQueue",
        &[
            ("EnqueuedURI", uri),
            ("EnqueuedURIMetaData", &meta),
            ("DesiredFirstTrackNumberEnqueued", "0"),
            ("EnqueueAsNext", "0"),
        ],
    );
    avt(
        &t,
        "SetAVTransportURI",
        &[
            ("CurrentURI", &format!("x-rincon-queue:{uuid}#0")),
            ("CurrentURIMetaData", ""),
        ],
    );
    avt(&t, "Play", &[("Speed", "1")]);
}

/// `room` plays a stream whose metadata names `art`.
fn plays_stream_with_art(sim: &SimHandle, room: &str, art: &str) {
    let t = sim.transport(room).unwrap();
    let meta = format!(
        "{DIDL_OPEN}<item id=\"-1\" parentID=\"-1\" restricted=\"true\"><dc:title>Radio</dc:title>\
         <upnp:class>object.item.audioItem.audioBroadcast</upnp:class>\
         <upnp:albumArtURI>{}</upnp:albumArtURI></item></DIDL-Lite>",
        art.replace('&', "&amp;")
    );
    avt(
        &t,
        "SetAVTransportURI",
        &[
            (
                "CurrentURI",
                "x-rincon-mp3radio://stream.example.invalid/radio.mp3",
            ),
            ("CurrentURIMetaData", &meta),
        ],
    );
    avt(&t, "Play", &[("Speed", "1")]);
}

#[test]
fn art_comes_only_from_the_zones_own_player() {
    let sim = SimHousehold::standard().spawn().expect("sim");
    let remote = Remote::start(&sim);
    let kitchen = sim.player("Kitchen").unwrap().ip;

    // Nothing playing: no art, nothing fetched.
    let none = get(&remote, "/art?zone=Kitchen", &[]);
    assert_eq!(none.status, 204, "{}", none.text());
    assert_eq!(remote.fetched(), Vec::<String>::new());

    kitchen_plays_spotify(&sim);
    let art = get(
        &remote,
        "/art?zone=Kitchen&v=t1",
        &[("Sec-Fetch-Site", "same-origin")],
    );
    assert_eq!(art.status, 200, "{}", art.text());
    assert_eq!(art.header("content-type"), Some("image/png"));
    assert_eq!(
        art.header("cross-origin-resource-policy"),
        Some("same-origin")
    );
    assert!(art.body.starts_with(b"\x89PNG"));
    let fetched = remote.fetched();
    assert_eq!(fetched.len(), 1, "{fetched:?}");
    assert!(
        fetched[0].starts_with(&format!("http://{kitchen}:1400/getaa?s=1&u=")),
        "{fetched:?}"
    );

    // A caller-supplied URL is refused before anything is fetched.
    for supplied in [
        "/art?zone=Kitchen&url=http%3A%2F%2F127.0.0.1%3A9%2Fsecret",
        "/art?zone=Kitchen&uri=http://192.0.2.99:1400/getaa",
        "/art?url=http://169.254.169.254/latest/meta-data",
    ] {
        let refused = get(&remote, supplied, &[]);
        assert_eq!(
            (refused.status, refused.code().as_str()),
            (422, "INVALID_ARGUMENT"),
            "{supplied}"
        );
    }
    // Another site's page gets no art.
    let embedded = get(
        &remote,
        "/art?zone=Kitchen",
        &[("Sec-Fetch-Site", "cross-site")],
    );
    assert_eq!(
        (embedded.status, embedded.code().as_str()),
        (403, "UNTRUSTED_ORIGIN")
    );
    assert_eq!(
        remote.fetched(),
        Vec::<String>::new(),
        "nothing was fetched"
    );

    // Art the player reports elsewhere (a service's CDN, another player)
    // is not fetched.
    for elsewhere in [
        "https://art.example.invalid/cover.jpg".to_owned(),
        format!("http://{kitchen}:1400/getaa?s=1&u=x"),
    ] {
        plays_stream_with_art(&sim, "Office", &elsewhere);
        let answer = get(&remote, "/art?zone=Office", &[]);
        assert_eq!(answer.status, 204, "{elsewhere}: {}", answer.text());
        assert_eq!(remote.fetched(), Vec::<String>::new(), "{elsewhere}");
    }
}
