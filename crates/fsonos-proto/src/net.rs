//! The real LAN transport over asupersync: SOAP POST and HTTP GET to players,
//! the SSDP `M-SEARCH`, GENA SUBSCRIBE / renew / UNSUBSCRIBE, and the
//! [`EventSink`] that receives the players' NOTIFY callbacks (and can serve
//! them announcement clips, see [`MediaFiles`]).
//!
//! One worker thread owns a current-thread asupersync runtime and runs each
//! request to completion, in order. Callers are ordinary synchronous code (the
//! protocol and core logic stay pure), and because they never run on the
//! worker's thread they may call in from anywhere, including from inside
//! another asupersync runtime such as the MCP server's, without nesting
//! `block_on`. A household is a handful of players, so serial requests are
//! plenty.

use crate::gena::{self, Notify, Subscription};
use crate::mdns;
use crate::ssdp::{self, Advert};
use crate::{HttpBody, MAX_BODY_BYTES, ProtoError, Transport};
use asupersync::Cx;
use asupersync::http::Client;
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::{Method, Request, Response};
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use asupersync::net::UdpSocket;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use asupersync::server::shutdown::ShutdownSignal;
use asupersync::time::{timeout, wall_now};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The port every player serves its UPnP control and description on.
pub const PLAYER_PORT: u16 = 1400;

/// How long one request may take before it fails as [`ProtoError::Network`].
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

/// A handle to the LAN worker. Cheap to share by reference; dropping it stops
/// the worker after any request in flight.
pub struct Lan {
    jobs: Option<mpsc::Sender<Job>>,
    worker: Option<JoinHandle<()>>,
    timeout: Duration,
    routes: Vec<(IpAddr, SocketAddr)>,
    ssdp_target: SocketAddr,
    mdns_target: SocketAddr,
}

enum Job {
    Http {
        method: Method,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        timeout: Duration,
        reply: mpsc::Sender<Result<Reply, ProtoError>>,
    },
    Search {
        target: SocketAddr,
        mx_secs: u8,
        wait: Duration,
        reply: mpsc::Sender<Result<Vec<Advert>, ProtoError>>,
    },
    MdnsSearch {
        target: SocketAddr,
        wait: Duration,
        reply: mpsc::Sender<Result<Vec<mdns::SonosAdvert>, ProtoError>>,
    },
    Route {
        toward: SocketAddr,
        reply: mpsc::Sender<Result<IpAddr, ProtoError>>,
    },
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    /// The body as text (SOAP, descriptions): invalid UTF-8 is replaced.
    fn text(self) -> String {
        match String::from_utf8(self.body) {
            Ok(text) => text,
            Err(e) => String::from_utf8_lossy(e.as_bytes()).into_owned(),
        }
    }

    /// A header's value (names ignore case).
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

impl Lan {
    /// Start the worker with the [`DEFAULT_TIMEOUT`] per request.
    pub fn start() -> Result<Self, ProtoError> {
        Self::with_timeout(DEFAULT_TIMEOUT)
    }

    /// Start the worker; every request fails after `timeout`.
    pub fn with_timeout(timeout: Duration) -> Result<Self, ProtoError> {
        let (jobs, inbox) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let worker = thread::Builder::new()
            .name("fsonos-lan".into())
            .spawn(move || {
                let runtime = match new_runtime() {
                    Ok(rt) => {
                        let _ = ready_tx.send(Ok(()));
                        rt
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                for job in inbox {
                    runtime.block_on(run(job));
                }
            })
            .map_err(|e| network("LAN worker", e))?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                jobs: Some(jobs),
                worker: Some(worker),
                timeout,
                routes: Vec::new(),
                ssdp_target: SocketAddr::from(([239, 255, 255, 250], 1900)),
                mdns_target: mdns::MDNS_ADDR.parse().expect("literal group"),
            }),
            Ok(Err(e)) => Err(network("asupersync runtime", e)),
            Err(e) => Err(network("LAN worker", e)),
        }
    }

    /// Send traffic for these player addresses to other socket addresses
    /// instead of `ip:1400`: SOAP, description GETs, GENA, and the callback
    /// route. Test plumbing for a separate process (the real `fsonos` binary
    /// under test) to reach a simulated household whose players listen on
    /// loopback ports; real players always answer on `ip:1400`.
    #[must_use]
    pub fn with_routes(mut self, routes: Vec<(IpAddr, SocketAddr)>) -> Self {
        self.routes = routes;
        self
    }

    /// Send the SSDP `M-SEARCH` to `target` (e.g. a simulator's unicast
    /// responder) instead of the multicast group.
    #[must_use]
    pub fn with_ssdp_target(mut self, target: SocketAddr) -> Self {
        self.ssdp_target = target;
        self
    }

    /// Send the mDNS query to `target` (e.g. a simulator's unicast
    /// responder) instead of the multicast group. Test plumbing, the
    /// counterpart of [`Self::with_ssdp_target`].
    #[must_use]
    pub fn with_mdns_target(mut self, target: SocketAddr) -> Self {
        self.mdns_target = target;
        self
    }

    /// Where requests for the player at `host` go.
    fn endpoint(&self, host: IpAddr) -> SocketAddr {
        self.routes
            .iter()
            .find(|(ip, _)| *ip == host)
            .map_or(SocketAddr::new(host, PLAYER_PORT), |(_, to)| *to)
    }

    /// `url` with a routed player's `ip:1400` authority replaced.
    fn route_url(&self, url: &str) -> String {
        let Some(rest) = url.strip_prefix("http://") else {
            return url.to_string();
        };
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        match authority.parse::<SocketAddr>() {
            Ok(addr) if addr.port() == PLAYER_PORT => {
                format!("http://{}{path}", self.endpoint(addr.ip()))
            }
            _ => url.to_string(),
        }
    }

    fn event_url(&self, host: IpAddr, event_path: &str) -> String {
        format!("http://{}{event_path}", self.endpoint(host))
    }

    fn submit<T>(
        &self,
        make: impl FnOnce(mpsc::Sender<Result<T, ProtoError>>) -> Job,
    ) -> Result<T, ProtoError> {
        let (reply, answer) = mpsc::channel();
        self.jobs
            .as_ref()
            .ok_or_else(|| network("LAN worker", "stopped"))?
            .send(make(reply))
            .map_err(|e| network("LAN worker", e))?;
        answer.recv().map_err(|e| network("LAN worker", e))?
    }

    fn http(
        &self,
        method: Method,
        url: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    ) -> Result<Reply, ProtoError> {
        let timeout = self.timeout;
        self.submit(|reply| Job::Http {
            method,
            url,
            headers,
            body,
            timeout,
            reply,
        })
    }

    /// The local address this host uses to reach `player`: the address to
    /// give players as a GENA callback. Sends nothing.
    pub fn local_address_toward(&self, player: IpAddr) -> Result<IpAddr, ProtoError> {
        let toward = self.endpoint(player);
        self.submit(|reply| Job::Route { toward, reply })
    }

    /// SUBSCRIBE to the events `event_path` on the player at `host` publishes,
    /// delivered to `callback_url` (an [`EventSink`]'s). The player sends a
    /// full-state NOTIFY right away.
    pub fn subscribe(
        &self,
        host: IpAddr,
        event_path: &str,
        callback_url: &str,
        timeout_secs: u32,
    ) -> Result<Subscription, ProtoError> {
        self.subscribe_at(
            &self.event_url(host, event_path),
            callback_url,
            timeout_secs,
        )
    }

    /// Renew subscription `sid` before it times out. A player that rebooted
    /// answers 412: subscribe afresh.
    pub fn renew(
        &self,
        host: IpAddr,
        event_path: &str,
        sid: &str,
        timeout_secs: u32,
    ) -> Result<Subscription, ProtoError> {
        self.renew_at(&self.event_url(host, event_path), sid, timeout_secs)
    }

    /// End subscription `sid`.
    pub fn unsubscribe(&self, host: IpAddr, event_path: &str, sid: &str) -> Result<(), ProtoError> {
        self.unsubscribe_at(&self.event_url(host, event_path), sid)
    }

    /// [`Lan::subscribe`] against a full event URL.
    pub fn subscribe_at(
        &self,
        event_url: &str,
        callback_url: &str,
        timeout_secs: u32,
    ) -> Result<Subscription, ProtoError> {
        self.gena(
            "SUBSCRIBE",
            event_url,
            gena::subscribe_headers(callback_url, timeout_secs),
        )
    }

    /// [`Lan::renew`] against a full event URL.
    pub fn renew_at(
        &self,
        event_url: &str,
        sid: &str,
        timeout_secs: u32,
    ) -> Result<Subscription, ProtoError> {
        self.gena(
            "SUBSCRIBE",
            event_url,
            gena::renew_headers(sid, timeout_secs),
        )
    }

    /// [`Lan::unsubscribe`] against a full event URL.
    pub fn unsubscribe_at(&self, event_url: &str, sid: &str) -> Result<(), ProtoError> {
        let reply = self.http(
            Method::Extension("UNSUBSCRIBE".into()),
            self.route_url(event_url),
            vec![("SID".into(), sid.into())],
            Vec::new(),
        )?;
        match reply.status {
            200 => Ok(()),
            status => Err(network(event_url, format!("UNSUBSCRIBE: HTTP {status}"))),
        }
    }

    fn gena(
        &self,
        method: &str,
        event_url: &str,
        headers: Vec<(String, String)>,
    ) -> Result<Subscription, ProtoError> {
        let reply = self.http(
            Method::Extension(method.into()),
            self.route_url(event_url),
            headers,
            Vec::new(),
        )?;
        match reply.status {
            200 => gena::subscription_from_headers(&reply.headers),
            status => Err(network(event_url, format!("{method}: HTTP {status}"))),
        }
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        // Closing the channel ends the worker's loop.
        self.jobs.take();
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl Transport for Lan {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let url = format!("http://{}{control_path}", self.endpoint(host));
        let reply = self.http(
            Method::Post,
            url.clone(),
            vec![
                ("Content-Type".into(), "text/xml; charset=\"utf-8\"".into()),
                ("SOAPACTION".into(), soap_action.into()),
            ],
            body.as_bytes().to_vec(),
        )?;
        match reply.status {
            // Sonos reports UPnP faults as HTTP 500 with a SOAP Fault body,
            // which soap::parse_response turns into ProtoError::SoapFault.
            200 | 500 => Ok(reply.text()),
            status => Err(network(url, format!("HTTP {status}"))),
        }
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        let reply = self.http(Method::Get, self.route_url(url), Vec::new(), Vec::new())?;
        if reply.status == 200 {
            Ok(reply.text())
        } else {
            Err(network(url, format!("HTTP {}", reply.status)))
        }
    }

    /// Like [`Self::http_get`], routed the same way; a body over
    /// [`MAX_BODY_BYTES`] is an error.
    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        let reply = self.http(Method::Get, self.route_url(url), Vec::new(), Vec::new())?;
        if reply.status != 200 {
            return Err(network(url, format!("HTTP {}", reply.status)));
        }
        if reply.body.len() > MAX_BODY_BYTES {
            return Err(network(
                url,
                format!(
                    "the body is {} bytes, over the {MAX_BODY_BYTES}-byte limit",
                    reply.body.len()
                ),
            ));
        }
        Ok(HttpBody {
            content_type: reply.header("content-type").map(str::to_owned),
            body: reply.body,
        })
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        let target = self.ssdp_target;
        self.submit(|reply| Job::Search {
            target,
            mx_secs,
            wait,
            reply,
        })
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        let target = self.mdns_target;
        self.submit(|reply| Job::MdnsSearch {
            target,
            wait,
            reply,
        })
    }
}

async fn run(job: Job) {
    match job {
        Job::Http {
            method,
            url,
            headers,
            body,
            timeout,
            reply,
        } => {
            let _ = reply.send(http(method, url, headers, body, timeout).await);
        }
        Job::Search {
            target,
            mx_secs,
            wait,
            reply,
        } => {
            let _ = reply.send(search(target, mx_secs, wait).await);
        }
        Job::MdnsSearch {
            target,
            wait,
            reply,
        } => {
            let _ = reply.send(mdns_search(target, wait).await);
        }
        Job::Route { toward, reply } => {
            let _ = reply.send(route(toward).await);
        }
    }
}

fn new_runtime() -> Result<Runtime, String> {
    let reactor = create_reactor().map_err(|e| e.to_string())?;
    RuntimeBuilder::current_thread()
        .with_reactor(reactor)
        .build()
        .map_err(|e| e.to_string())
}

/// A connected UDP socket's local address is the one the routing table picks
/// for `toward`; connecting a UDP socket sends nothing.
async fn route(toward: SocketAddr) -> Result<IpAddr, ProtoError> {
    let unspecified: SocketAddr = if toward.is_ipv4() {
        "0.0.0.0:0".parse().expect("literal")
    } else {
        "[::]:0".parse().expect("literal")
    };
    let socket = UdpSocket::bind(unspecified)
        .await
        .map_err(|e| network("route", e))?;
    socket
        .connect(toward)
        .await
        .map_err(|e| network("route", e))?;
    Ok(socket.local_addr().map_err(|e| network("route", e))?.ip())
}

async fn http(
    method: Method,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<Reply, ProtoError> {
    let cx = Cx::current().ok_or_else(|| network(&url, "no asupersync context"))?;
    let client = Client::default_for_runtime(&cx);
    let mut request = client.request_builder(method, url.clone());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    if !body.is_empty() {
        request = request.body(body);
    }
    let response = request
        .timeout(timeout)
        .send(&cx)
        .await
        .map_err(|e| network(&url, e))?;
    Ok(Reply {
        status: response.status,
        headers: response.headers,
        body: response.body,
    })
}

/// Send the `M-SEARCH` (twice: UDP may drop one) and collect distinct Sonos
/// replies until `wait` has passed.
async fn search(
    target: SocketAddr,
    mx_secs: u8,
    wait: Duration,
) -> Result<Vec<Advert>, ProtoError> {
    let mut socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| network("SSDP", e))?;
    let message = ssdp::m_search(mx_secs);
    for _ in 0..2 {
        socket
            .send_to(message.as_bytes(), target)
            .await
            .map_err(|e| network("SSDP", e))?;
    }
    let started = Instant::now();
    let mut buf = vec![0u8; 4096];
    let mut found: Vec<Advert> = Vec::new();
    loop {
        let left = wait.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        match timeout(wall_now(), left, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _from))) => {
                if let Some(advert) = ssdp::parse_response(&buf[..n])
                    && advert.st.eq_ignore_ascii_case(ssdp::SONOS_ST)
                    && !found.iter().any(|f| f.location == advert.location)
                {
                    found.push(advert);
                }
            }
            Ok(Err(e)) => return Err(network("SSDP", e)),
            Err(_elapsed) => break,
        }
    }
    Ok(found)
}

/// Send the mDNS query for `_sonos._tcp.local` (twice: UDP may drop one)
/// and collect Sonos advertisements from the replies that arrive within
/// `wait`, deduplicated by instance name. Parse failures are skipped, not
/// errors: anything else on the group must not break discovery.
async fn mdns_search(
    target: SocketAddr,
    wait: Duration,
) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
    let mut socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| network("mDNS", e))?;
    let message = mdns::query();
    for _ in 0..2 {
        socket
            .send_to(&message, target)
            .await
            .map_err(|e| network("mDNS", e))?;
    }
    let started = Instant::now();
    let mut buf = vec![0u8; 4096];
    let mut found: Vec<mdns::SonosAdvert> = Vec::new();
    loop {
        let left = wait.saturating_sub(started.elapsed());
        if left.is_zero() {
            break;
        }
        match timeout(wall_now(), left, socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _from))) => {
                if let Ok(msg) = mdns::parse_message(&buf[..n]) {
                    for advert in mdns::sonos_adverts(&msg) {
                        if !found.iter().any(|f| f.instance == advert.instance) {
                            found.push(advert);
                        }
                    }
                }
            }
            Ok(Err(e)) => return Err(network("mDNS", e)),
            Err(_elapsed) => break,
        }
    }
    Ok(found)
}

fn network(target: impl Into<String>, detail: impl std::fmt::Display) -> ProtoError {
    ProtoError::Network {
        target: target.into(),
        detail: detail.to_string(),
    }
}

/// Files an [`EventSink`] serves read-only at `GET /media/<name>`: the file
/// `name` (one path segment) stands for, or `None` for a 404. The lookup
/// decides what may be served; the sink only reads what it names.
pub type MediaFiles = Arc<dyn Fn(&str) -> Option<PathBuf> + Send + Sync>;

/// Receives the players' GENA NOTIFY callbacks.
///
/// It listens on an address the players can reach (this host's LAN address,
/// see [`Lan::local_address_toward`]; not loopback) and, like every listener
/// here, accepts only requests whose Host is that address. Each NOTIFY is
/// answered 200 and queued for [`EventSink::recv_timeout`]. Bind a specific
/// address, not the wildcard: it is also the Host the players will send.
///
/// Started with [`MediaFiles`] ([`EventSink::start_serving`]), it also
/// answers `GET /media/<name>`, so the players fetch announcement clips from
/// the address they already send events to.
pub struct EventSink {
    addr: SocketAddr,
    events: mpsc::Receiver<Notify>,
    shutdown: ShutdownSignal,
    server: Option<JoinHandle<()>>,
}

impl EventSink {
    /// Listen on `bind`; port 0 picks a free one.
    pub fn start(bind: SocketAddr) -> Result<Self, ProtoError> {
        Self::start_serving(bind, None)
    }

    /// [`Self::start`], also serving `media` at `GET /media/<name>` when
    /// given. Without it, a GET is refused (405) like any other non-NOTIFY.
    pub fn start_serving(bind: SocketAddr, media: Option<MediaFiles>) -> Result<Self, ProtoError> {
        let (queue, events) = mpsc::channel::<Notify>();
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(SocketAddr, ShutdownSignal), String>>();
        let server = thread::Builder::new()
            .name("fsonos-gena-sink".into())
            .spawn(move || {
                let runtime = match new_runtime() {
                    Ok(rt) => rt,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let handle = runtime.handle();
                runtime.block_on(async move {
                    let config = Http1ListenerConfig::default().http_config(
                        Http1Config::default()
                            .host_policy(HostPolicy::allow_list(vec![bind.ip().to_string()])),
                    );
                    let handler = move |req: Request| {
                        let queue = queue.clone();
                        let media = media.clone();
                        async move {
                            match &media {
                                Some(files) if matches!(req.method, Method::Get) => {
                                    serve_media(files, &req.uri)
                                }
                                _ => accept_notify(&req, &queue),
                            }
                        }
                    };
                    let listener =
                        match Http1Listener::bind_with_config(bind, handler, config).await {
                            Ok(listener) => listener,
                            Err(e) => {
                                let _ = ready_tx.send(Err(e.to_string()));
                                return;
                            }
                        };
                    let ready = listener
                        .local_addr()
                        .map(|addr| (addr, listener.shutdown_signal()))
                        .map_err(|e| e.to_string());
                    let ok = ready.is_ok();
                    let _ = ready_tx.send(ready);
                    if ok {
                        let _ = listener.run(&handle).await;
                    }
                });
            })
            .map_err(|e| network("GENA sink", e))?;
        match ready_rx.recv() {
            Ok(Ok((addr, shutdown))) => Ok(Self {
                addr,
                events,
                shutdown,
                server: Some(server),
            }),
            Ok(Err(e)) => Err(network(format!("GENA sink on {bind}"), e)),
            Err(e) => Err(network("GENA sink", e)),
        }
    }

    /// The address the sink listens on.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// The CALLBACK URL to subscribe with. `tag` (e.g. the service name)
    /// comes back as [`Notify::path`], so one sink can serve every service.
    #[must_use]
    pub fn callback_url(&self, tag: &str) -> String {
        format!("http://{}/{}", self.addr, tag.trim_start_matches('/'))
    }

    /// The next NOTIFY, waiting up to `wait`.
    #[must_use]
    pub fn recv_timeout(&self, wait: Duration) -> Option<Notify> {
        self.events.recv_timeout(wait).ok()
    }

    /// The next NOTIFY within `wait` (`Ok(None)`: none came), or an error
    /// once the listener has stopped and will deliver nothing more.
    pub fn recv_or_closed(&self, wait: Duration) -> Result<Option<Notify>, ProtoError> {
        match self.events.recv_timeout(wait) {
            Ok(n) => Ok(Some(n)),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(ProtoError::Network {
                target: format!("event listener {}", self.addr),
                detail: "stopped".into(),
            }),
        }
    }
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.shutdown.trigger_immediate();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}

/// Answer `GET /media/<name>` with the file `files` names for it.
fn serve_media(files: &MediaFiles, uri: &str) -> Response {
    let path = uri.split(['?', '#']).next().unwrap_or_default();
    let file = path
        .strip_prefix("/media/")
        .filter(|name| !name.is_empty() && !name.contains('/'))
        .and_then(|name| files(name));
    let Some(file) = file else {
        return Response::new(404, "Not Found", Vec::new());
    };
    let kind = match file.extension().and_then(|e| e.to_str()) {
        Some(e) if e.eq_ignore_ascii_case("wav") => "audio/wav",
        _ => "application/octet-stream",
    };
    match std::fs::read(&file) {
        Ok(body) => Response::new(200, "OK", body).with_header("Content-Type", kind),
        Err(_) => Response::new(404, "Not Found", Vec::new()),
    }
}

fn accept_notify(req: &Request, queue: &mpsc::Sender<Notify>) -> Response {
    if !matches!(&req.method, Method::Extension(m) if m.eq_ignore_ascii_case("NOTIFY")) {
        return Response::new(405, "Method Not Allowed", Vec::new());
    }
    let header = |name: &str| {
        req.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim().to_string())
    };
    // UPnP: a NOTIFY without a SID (or NT/NTS) is answered 412.
    let Some(sid) = header("SID").filter(|s| !s.is_empty()) else {
        return Response::new(412, "Precondition Failed", Vec::new());
    };
    let seq = header("SEQ").and_then(|s| s.parse().ok()).unwrap_or(0);
    match gena::parse_propertyset(&String::from_utf8_lossy(&req.body)) {
        Ok(properties) => {
            let _ = queue.send(Notify {
                sid,
                seq,
                path: req.uri.clone(),
                properties,
            });
            Response::new(200, "OK", Vec::new())
        }
        Err(_) => Response::new(400, "Bad Request", Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpStream;

    fn runtime() -> Runtime {
        RuntimeBuilder::current_thread()
            .with_reactor(create_reactor().expect("reactor"))
            .build()
            .expect("runtime")
    }

    /// A loopback HTTP server that answers SOAP-ish POSTs with a fixed status
    /// and echoes the SOAPACTION header, and GETs with "desc <path>" (but
    /// `/getaa` with [`ART`], and `/huge` with one byte over the limit).
    fn serve(status: u16) -> (SocketAddr, impl FnOnce()) {
        let config = Http1ListenerConfig::default().http_config(
            Http1Config::default().host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let rt = runtime();
            let handle = rt.handle();
            rt.block_on(async move {
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:0",
                    move |req: Request| async move {
                        let action = req
                            .headers
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case("SOAPACTION"))
                            .map(|(_, v)| v.clone());
                        let body = match action {
                            Some(a) => format!("{a} {}", String::from_utf8_lossy(&req.body)),
                            None if req.uri.starts_with("/getaa") => {
                                return Response::new(status, "X", ART.to_vec())
                                    .with_header("Content-Type", "image/png");
                            }
                            None if req.uri == "/huge" => {
                                return Response::new(status, "X", vec![0; MAX_BODY_BYTES + 1]);
                            }
                            None => format!("desc {}", req.uri),
                        };
                        Response::new(status, "X", body.into_bytes())
                    },
                    config,
                )
                .await
                .expect("bind");
                ready_tx
                    .send((
                        listener.local_addr().expect("addr"),
                        listener.shutdown_signal(),
                    ))
                    .expect("ready");
                listener.run(&handle).await.expect("run");
            });
        });
        let (addr, shutdown) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("ready");
        (addr, move || {
            shutdown.trigger_immediate();
            server.join().expect("server thread");
        })
    }

    #[test]
    fn get_and_post_reach_a_loopback_server() {
        let (addr, stop) = serve(200);
        let lan = Lan::start().expect("lan");
        let body = lan
            .http_get(&format!("http://{addr}/xml/device_description.xml"))
            .expect("get");
        assert_eq!(body, "desc /xml/device_description.xml");

        let reply = lan
            .http(
                Method::Post,
                format!("http://{addr}/MediaRenderer/AVTransport/Control"),
                vec![("SOAPACTION".into(), "\"urn:x#Play\"".into())],
                b"<env/>".to_vec(),
            )
            .expect("post");
        assert_eq!(reply.status, 200);
        assert_eq!(reply.text(), "\"urn:x#Play\" <env/>");
        drop(lan);
        stop();
    }

    /// Bytes that are not UTF-8, as images are.
    const ART: &[u8] = &[0x89, b'P', b'N', b'G', 0xff, 0x00, 0xfe];

    #[test]
    fn bytes_arrive_intact_routed_and_capped() {
        let (addr, stop) = serve(200);
        let player: IpAddr = "192.0.2.10".parse().unwrap();
        let lan = Lan::start().expect("lan").with_routes(vec![(player, addr)]);
        let art = lan
            .http_get_bytes("http://192.0.2.10:1400/getaa?s=1&u=x")
            .expect("art");
        assert_eq!(art.body, ART);
        assert_eq!(art.content_type.as_deref(), Some("image/png"));
        match lan.http_get_bytes(&format!("http://{addr}/huge")) {
            Err(ProtoError::Network { detail, .. }) => {
                assert!(detail.contains("over the"), "{detail}");
            }
            other => panic!("expected the size limit, got {other:?}"),
        }
        // Text callers still read text.
        assert_eq!(
            lan.http_get("http://192.0.2.10:1400/xml/device_description.xml")
                .expect("get"),
            "desc /xml/device_description.xml"
        );
        drop(lan);
        stop();
    }

    /// A transport that does not fetch bytes refuses rather than guessing.
    #[test]
    fn bytes_are_not_wired_by_default() {
        struct SoapOnly;
        impl Transport for SoapOnly {
            fn soap_post(
                &self,
                _: IpAddr,
                _: &str,
                _: &str,
                _: &str,
            ) -> Result<String, ProtoError> {
                Ok(String::new())
            }
        }
        assert!(matches!(
            SoapOnly.http_get_bytes("http://192.0.2.10:1400/getaa"),
            Err(ProtoError::NotWired("http_get_bytes"))
        ));
    }

    #[test]
    fn non_200_get_and_refused_connection_are_network_errors() {
        let (addr, stop) = serve(404);
        let lan = Lan::with_timeout(Duration::from_secs(2)).expect("lan");
        match lan.http_get(&format!("http://{addr}/missing")) {
            Err(ProtoError::Network { detail, .. }) => assert_eq!(detail, "HTTP 404"),
            other => panic!("expected a network error, got {other:?}"),
        }
        stop();
        // The listener is gone: nothing answers on that port any more.
        assert!(matches!(
            lan.http_get(&format!("http://{addr}/x")),
            Err(ProtoError::Network { .. })
        ));
    }

    #[test]
    fn calls_from_inside_another_runtime_do_not_deadlock() {
        let (addr, stop) = serve(200);
        let lan = Lan::start().expect("lan");
        let body = runtime()
            .block_on(async { lan.http_get(&format!("http://{addr}/inside")).expect("get") });
        assert_eq!(body, "desc /inside");
        stop();
    }

    const RCS_VOLUME: &str = include_str!("../tests/fixtures/gena_notify_rcs_volume_s1.xml");

    /// Send one raw HTTP request and return the status line.
    fn raw(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut reply = String::new();
        let _ = stream.read_to_string(&mut reply);
        reply.lines().next().unwrap_or_default().to_string()
    }

    fn notify_request(addr: SocketAddr, host: &str, sid: Option<&str>, body: &str) -> String {
        let sid = sid.map(|s| format!("SID: {s}\r\n")).unwrap_or_default();
        format!(
            "NOTIFY /RenderingControl HTTP/1.1\r\nHost: {host}\r\n\
             Content-Type: text/xml; charset=\"utf-8\"\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\n\
             {sid}SEQ: 1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .replace("{addr}", &addr.to_string())
    }

    #[test]
    fn event_sink_queues_notifies_and_refuses_everything_else() {
        let sink = EventSink::start("127.0.0.1:0".parse().unwrap()).expect("sink");
        let addr = sink.local_addr();
        let host = addr.to_string();
        assert_eq!(
            sink.callback_url("/RenderingControl"),
            format!("http://{addr}/RenderingControl")
        );

        let status = raw(
            addr,
            &notify_request(addr, &host, Some("uuid:RINCON_X_sub7"), RCS_VOLUME),
        );
        assert!(status.contains(" 200"), "{status}");
        let n = sink.recv_timeout(Duration::from_secs(5)).expect("a NOTIFY");
        assert_eq!(
            (n.sid.as_str(), n.seq, n.path.as_str()),
            ("uuid:RINCON_X_sub7", 1, "/RenderingControl")
        );
        assert_eq!(n.last_change().unwrap().unwrap().volume(), Some(19));

        let missing_sid = raw(addr, &notify_request(addr, &host, None, RCS_VOLUME));
        assert!(missing_sid.contains(" 412"), "{missing_sid}");
        let foreign = raw(
            addr,
            &notify_request(addr, "player.example", Some("uuid:x"), RCS_VOLUME),
        );
        assert!(
            foreign.contains(" 421"),
            "a foreign Host is refused: {foreign}"
        );
        let get = raw(
            addr,
            &format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
        );
        assert!(get.contains(" 405"), "{get}");
        assert!(
            sink.recv_timeout(Duration::from_millis(50)).is_none(),
            "only the good NOTIFY queued"
        );
    }

    /// The whole reply to `request`.
    fn raw_reply(addr: SocketAddr, request: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("timeout");
        stream.write_all(request.as_bytes()).expect("write");
        let mut reply = Vec::new();
        let _ = stream.read_to_end(&mut reply);
        String::from_utf8_lossy(&reply).into_owned()
    }

    #[test]
    fn event_sink_serves_media_and_still_takes_notifies() {
        let dir = std::env::temp_dir().join(format!("fsonos-sink-media-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let clip = dir.join("0123abcd.wav");
        std::fs::write(&clip, b"RIFF-a-test-clip").unwrap();
        let served = clip.clone();
        let files: MediaFiles =
            Arc::new(move |name| (name == "0123abcd.wav").then(|| served.clone()));
        let sink =
            EventSink::start_serving("127.0.0.1:0".parse().unwrap(), Some(files)).expect("sink");
        let addr = sink.local_addr();
        let host = addr.to_string();
        let get = |path: &str| {
            raw_reply(
                addr,
                &format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"),
            )
        };

        let reply = get("/media/0123abcd.wav");
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert!(
            reply
                .to_ascii_lowercase()
                .contains("content-type: audio/wav"),
            "{reply}"
        );
        assert!(reply.ends_with("RIFF-a-test-clip"), "{reply}");
        assert!(get("/media/0123abcd.wav?x=1").starts_with("HTTP/1.1 200"));
        for missing in [
            "/media/ffff.wav",
            "/media/",
            "/media/../0123abcd.wav",
            "/0123abcd.wav",
            "/",
        ] {
            let reply = get(missing);
            assert!(reply.starts_with("HTTP/1.1 404"), "{missing}: {reply}");
        }
        let foreign = raw_reply(
            addr,
            "GET /media/0123abcd.wav HTTP/1.1\r\nHost: player.example\r\nConnection: close\r\n\r\n",
        );
        assert!(
            foreign.contains(" 421"),
            "a foreign Host is refused: {foreign}"
        );

        let status = raw(
            addr,
            &notify_request(addr, &host, Some("uuid:RINCON_X_sub8"), RCS_VOLUME),
        );
        assert!(status.contains(" 200"), "{status}");
        assert!(sink.recv_timeout(Duration::from_secs(5)).is_some());
        drop(sink);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Each request a fake player saw: its method and GENA headers.
    type Seen = mpsc::Receiver<(String, Vec<(String, String)>)>;

    /// A stand-in player that answers SUBSCRIBE and UNSUBSCRIBE and reports
    /// each request's method and GENA headers.
    fn fake_player() -> (SocketAddr, Seen, impl FnOnce()) {
        let config = Http1ListenerConfig::default().http_config(
            Http1Config::default().host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
        );
        let (seen_tx, seen_rx) = mpsc::channel();
        let (ready_tx, ready_rx) = mpsc::channel();
        let server = thread::spawn(move || {
            let rt = runtime();
            let handle = rt.handle();
            rt.block_on(async move {
                let handler = move |req: Request| {
                    let seen = seen_tx.clone();
                    async move {
                        let gena: Vec<(String, String)> = req
                            .headers
                            .iter()
                            .filter(|(k, _)| {
                                ["SID", "CALLBACK", "NT", "TIMEOUT"]
                                    .iter()
                                    .any(|h| k.eq_ignore_ascii_case(h))
                            })
                            .map(|(k, v)| (k.to_ascii_uppercase(), v.clone()))
                            .collect();
                        let method = req.method.as_str().to_string();
                        let _ = seen.send((method.clone(), gena));
                        match method.as_str() {
                            "SUBSCRIBE" => Response::new(200, "OK", Vec::new())
                                .with_header("SID", "uuid:RINCON_000E58A0000401400_sub0000000042")
                                .with_header("TIMEOUT", "Second-300"),
                            "UNSUBSCRIBE" => Response::new(200, "OK", Vec::new()),
                            _ => Response::new(405, "X", Vec::new()),
                        }
                    }
                };
                let listener = Http1Listener::bind_with_config("127.0.0.1:0", handler, config)
                    .await
                    .expect("bind");
                ready_tx
                    .send((
                        listener.local_addr().expect("addr"),
                        listener.shutdown_signal(),
                    ))
                    .expect("ready");
                listener.run(&handle).await.expect("run");
            });
        });
        let (addr, shutdown) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("ready");
        (addr, seen_rx, move || {
            shutdown.trigger_immediate();
            server.join().expect("player thread");
        })
    }

    #[test]
    fn subscribe_renew_unsubscribe_send_the_gena_headers() {
        let (addr, seen, stop) = fake_player();
        let lan = Lan::start().expect("lan");
        let url = format!("http://{addr}/MediaRenderer/RenderingControl/Event");

        let sub = lan
            .subscribe_at(&url, "http://192.0.2.1:3400/RenderingControl", 300)
            .expect("subscribe");
        assert_eq!(sub.sid, "uuid:RINCON_000E58A0000401400_sub0000000042");
        assert_eq!(sub.timeout_secs, 300);
        let (method, headers) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "SUBSCRIBE");
        assert!(headers.contains(&(
            "CALLBACK".into(),
            "<http://192.0.2.1:3400/RenderingControl>".into()
        )));
        assert!(headers.contains(&("NT".into(), "upnp:event".into())));

        lan.renew_at(&url, &sub.sid, 600).expect("renew");
        let (method, headers) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "SUBSCRIBE");
        assert!(headers.contains(&("SID".into(), sub.sid.clone())));
        assert!(
            !headers.iter().any(|(k, _)| k == "CALLBACK" || k == "NT"),
            "renewals carry neither"
        );

        lan.unsubscribe_at(&url, &sub.sid).expect("unsubscribe");
        let (method, headers) = seen.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(method, "UNSUBSCRIBE");
        assert_eq!(headers, [("SID".into(), sub.sid.clone())]);
        drop(lan);
        stop();
    }

    #[test]
    fn route_toward_loopback_is_loopback() {
        let lan = Lan::start().expect("lan");
        let local = lan
            .local_address_toward("127.0.0.1".parse().unwrap())
            .expect("route");
        assert!(local.is_loopback(), "{local}");
    }

    #[test]
    fn routes_send_a_players_traffic_to_its_socket() {
        let (addr, stop) = serve(200);
        let player: IpAddr = "192.0.2.77".parse().unwrap();
        let lan = Lan::start().expect("lan").with_routes(vec![(player, addr)]);

        let body = lan
            .soap_post(
                player,
                "/MediaRenderer/AVTransport/Control",
                "\"urn:x#Play\"",
                "<env/>",
            )
            .expect("routed SOAP");
        assert_eq!(body, "\"urn:x#Play\" <env/>");
        let desc = lan
            .http_get(&ssdp::description_url(player))
            .expect("routed GET");
        assert_eq!(desc, "desc /xml/device_description.xml");
        assert!(
            lan.local_address_toward(player)
                .expect("route")
                .is_loopback()
        );

        // Unrouted players and other ports are left alone.
        assert_eq!(
            lan.route_url("http://192.0.2.88:1400/x"),
            "http://192.0.2.88:1400/x"
        );
        assert_eq!(
            lan.route_url("http://192.0.2.77:8080/x"),
            "http://192.0.2.77:8080/x"
        );
        assert_eq!(
            lan.route_url("http://192.0.2.77:1400/x"),
            format!("http://{addr}/x")
        );
        drop(lan);
        stop();
    }

    #[test]
    fn ssdp_target_sends_the_search_to_a_unicast_responder() {
        let responder = std::net::UdpSocket::bind("127.0.0.1:0").expect("responder");
        let target = responder.local_addr().expect("addr");
        responder
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let answering = thread::spawn(move || {
            let mut buf = [0u8; 1024];
            // Answer both copies of the M-SEARCH; the client dedupes them.
            for _ in 0..2 {
                let Ok((n, from)) = responder.recv_from(&mut buf) else {
                    return;
                };
                assert!(String::from_utf8_lossy(&buf[..n]).starts_with("M-SEARCH * HTTP/1.1"));
                let reply = format!(
                    "HTTP/1.1 200 OK\r\nLOCATION: http://192.0.2.77:1400/xml/device_description.xml\r\n\
                     ST: {}\r\n\r\n",
                    ssdp::SONOS_ST
                );
                responder.send_to(reply.as_bytes(), from).expect("reply");
            }
        });
        let lan = Lan::start().expect("lan").with_ssdp_target(target);
        let found = lan.ssdp_search(1, Duration::from_secs(1)).expect("search");
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].location,
            "http://192.0.2.77:1400/xml/device_description.xml"
        );
        answering.join().expect("responder thread");
    }
}

#[test]
fn mdns_target_sends_the_query_and_parses_the_fixture_reply() {
    let fixture = include_bytes!("../tests/fixtures/mdns_response_s2.bin");
    let responder = std::net::UdpSocket::bind("127.0.0.1:0").expect("responder");
    let target = responder.local_addr().expect("addr");
    responder
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let answering = thread::spawn(move || {
        let mut buf = [0u8; 1024];
        for _ in 0..2 {
            let Ok((n, from)) = responder.recv_from(&mut buf) else {
                return;
            };
            // The QU question, byte for byte.
            assert_eq!(&buf[..n], mdns::query().as_slice());
            responder.send_to(fixture, from).expect("reply");
        }
    });
    let lan = Lan::start().expect("lan").with_mdns_target(target);
    let found = lan
        .mdns_search(Duration::from_secs(1))
        .expect("mdns search");
    assert_eq!(found.len(), 1, "the duplicated reply dedupes");
    let advert = &found[0];
    assert!(advert.instance.contains("RINCON_"), "{:?}", advert);
    assert!(
        advert
            .household
            .as_deref()
            .unwrap_or("")
            .starts_with("Sonos_")
    );
    assert!(advert.addr.is_some());
    answering.join().expect("responder thread");
}

#[test]
fn mdns_target_sends_the_query_and_parses_the_fixture_reply_s1() {
    let fixture = include_bytes!("../tests/fixtures/mdns_response_s1.bin");
    let responder = std::net::UdpSocket::bind("127.0.0.1:0").expect("responder");
    let target = responder.local_addr().expect("addr");
    responder
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("timeout");
    let answering = thread::spawn(move || {
        let mut buf = [0u8; 1024];
        for _ in 0..2 {
            let Ok((n, from)) = responder.recv_from(&mut buf) else {
                return;
            };
            assert_eq!(&buf[..n], mdns::query().as_slice());
            responder.send_to(fixture, from).expect("reply");
        }
    });
    let lan = Lan::start().expect("lan").with_mdns_target(target);
    let found = lan
        .mdns_search(Duration::from_secs(1))
        .expect("mdns search");
    assert_eq!(found.len(), 1, "the duplicated reply dedupes");
    let advert = &found[0];
    assert!(advert.instance.starts_with("Sonos-"), "{:?}", advert);
    assert!(advert.uuid.is_some(), "parsed from the instance name");
    answering.join().expect("responder thread");
}
