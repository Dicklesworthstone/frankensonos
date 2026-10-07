//! The real LAN transport over asupersync: SOAP POST and HTTP GET to players,
//! the SSDP `M-SEARCH`, GENA SUBSCRIBE / renew / UNSUBSCRIBE, and the
//! [`EventSink`] that receives the players' NOTIFY callbacks.
//!
//! One worker thread owns a current-thread asupersync runtime and runs each
//! request to completion, in order. Callers are ordinary synchronous code (the
//! protocol and core logic stay pure), and because they never run on the
//! worker's thread they may call in from anywhere, including from inside
//! another asupersync runtime such as the MCP server's, without nesting
//! `block_on`. A household is a handful of players, so serial requests are
//! plenty.

use crate::gena::{self, Notify, Subscription};
use crate::ssdp::{self, Advert};
use crate::{ProtoError, Transport};
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
use std::sync::mpsc;
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
        mx_secs: u8,
        wait: Duration,
        reply: mpsc::Sender<Result<Vec<Advert>, ProtoError>>,
    },
    Route {
        toward: IpAddr,
        reply: mpsc::Sender<Result<IpAddr, ProtoError>>,
    },
}

struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
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
            }),
            Ok(Err(e)) => Err(network("asupersync runtime", e)),
            Err(e) => Err(network("LAN worker", e)),
        }
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
        self.submit(|reply| Job::Route {
            toward: player,
            reply,
        })
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
        self.subscribe_at(&event_url(host, event_path), callback_url, timeout_secs)
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
        self.renew_at(&event_url(host, event_path), sid, timeout_secs)
    }

    /// End subscription `sid`.
    pub fn unsubscribe(&self, host: IpAddr, event_path: &str, sid: &str) -> Result<(), ProtoError> {
        self.unsubscribe_at(&event_url(host, event_path), sid)
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
            event_url.to_string(),
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
            event_url.to_string(),
            headers,
            Vec::new(),
        )?;
        match reply.status {
            200 => gena::subscription_from_headers(&reply.headers),
            status => Err(network(event_url, format!("{method}: HTTP {status}"))),
        }
    }
}

fn event_url(host: IpAddr, event_path: &str) -> String {
    format!("http://{}{event_path}", SocketAddr::new(host, PLAYER_PORT))
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
        let url = format!(
            "http://{}{control_path}",
            SocketAddr::new(host, PLAYER_PORT)
        );
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
            200 | 500 => Ok(reply.body),
            status => Err(network(url, format!("HTTP {status}"))),
        }
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        let reply = self.http(Method::Get, url.to_string(), Vec::new(), Vec::new())?;
        if reply.status == 200 {
            Ok(reply.body)
        } else {
            Err(network(url, format!("HTTP {}", reply.status)))
        }
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        self.submit(|reply| Job::Search {
            mx_secs,
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
            mx_secs,
            wait,
            reply,
        } => {
            let _ = reply.send(search(mx_secs, wait).await);
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
async fn route(toward: IpAddr) -> Result<IpAddr, ProtoError> {
    let unspecified: SocketAddr = if toward.is_ipv4() {
        "0.0.0.0:0".parse().expect("literal")
    } else {
        "[::]:0".parse().expect("literal")
    };
    let socket = UdpSocket::bind(unspecified)
        .await
        .map_err(|e| network("route", e))?;
    socket
        .connect(SocketAddr::new(toward, PLAYER_PORT))
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
        body: String::from_utf8_lossy(&response.body).into_owned(),
    })
}

/// Send the `M-SEARCH` (twice: UDP may drop one) and collect distinct Sonos
/// replies until `wait` has passed.
async fn search(mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
    let mut socket = UdpSocket::bind("0.0.0.0:0")
        .await
        .map_err(|e| network("SSDP", e))?;
    let message = ssdp::m_search(mx_secs);
    for _ in 0..2 {
        socket
            .send_to(message.as_bytes(), ssdp::SSDP_ADDR)
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

fn network(target: impl Into<String>, detail: impl std::fmt::Display) -> ProtoError {
    ProtoError::Network {
        target: target.into(),
        detail: detail.to_string(),
    }
}

/// Receives the players' GENA NOTIFY callbacks.
///
/// It listens on an address the players can reach (this host's LAN address,
/// see [`Lan::local_address_toward`]; not loopback) and, like every listener
/// here, accepts only requests whose Host is that address. Each NOTIFY is
/// answered 200 and queued for [`EventSink::recv_timeout`]. Bind a specific
/// address, not the wildcard: it is also the Host the players will send.
pub struct EventSink {
    addr: SocketAddr,
    events: mpsc::Receiver<Notify>,
    shutdown: ShutdownSignal,
    server: Option<JoinHandle<()>>,
}

impl EventSink {
    /// Listen on `bind`; port 0 picks a free one.
    pub fn start(bind: SocketAddr) -> Result<Self, ProtoError> {
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
                        async move { accept_notify(&req, &queue) }
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
}

impl Drop for EventSink {
    fn drop(&mut self) {
        self.shutdown.trigger_immediate();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
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
    /// and echoes the SOAPACTION header, and GETs with "desc <path>".
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
        assert_eq!(reply.body, "\"urn:x#Play\" <env/>");
        drop(lan);
        stop();
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
}
