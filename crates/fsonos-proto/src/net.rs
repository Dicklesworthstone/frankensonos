//! The real LAN transport: SOAP POST and HTTP GET to players, and the SSDP
//! `M-SEARCH`, over asupersync.
//!
//! One worker thread owns a current-thread asupersync runtime and runs each
//! request to completion, in order. Callers are ordinary synchronous code (the
//! protocol and core logic stay pure), and because they never run on the
//! worker's thread they may call in from anywhere, including from inside
//! another asupersync runtime such as the MCP server's, without nesting
//! `block_on`. A household is a handful of players, so serial requests are
//! plenty.

use crate::ssdp::{self, Advert};
use crate::{ProtoError, Transport};
use asupersync::Cx;
use asupersync::http::Client;
use asupersync::net::UdpSocket;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
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
        post: bool,
        url: String,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
        timeout: Duration,
        reply: mpsc::Sender<Result<Reply, ProtoError>>,
    },
    Search {
        mx_secs: u8,
        wait: Duration,
        reply: mpsc::Sender<Result<Vec<Advert>, ProtoError>>,
    },
}

struct Reply {
    status: u16,
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
                let runtime = create_reactor()
                    .map_err(|e| e.to_string())
                    .and_then(|reactor| {
                        RuntimeBuilder::current_thread()
                            .with_reactor(reactor)
                            .build()
                            .map_err(|e| e.to_string())
                    });
                let runtime = match runtime {
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
        post: bool,
        url: String,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    ) -> Result<Reply, ProtoError> {
        let timeout = self.timeout;
        self.submit(|reply| Job::Http {
            post,
            url,
            headers,
            body,
            timeout,
            reply,
        })
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
        let url = format!(
            "http://{}{control_path}",
            SocketAddr::new(host, PLAYER_PORT)
        );
        let reply = self.http(
            true,
            url.clone(),
            vec![
                ("Content-Type", "text/xml; charset=\"utf-8\"".into()),
                ("SOAPACTION", soap_action.into()),
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
        let reply = self.http(false, url.to_string(), Vec::new(), Vec::new())?;
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
            post,
            url,
            headers,
            body,
            timeout,
            reply,
        } => {
            let _ = reply.send(http(post, url, headers, body, timeout).await);
        }
        Job::Search {
            mx_secs,
            wait,
            reply,
        } => {
            let _ = reply.send(search(mx_secs, wait).await);
        }
    }
}

async fn http(
    post: bool,
    url: String,
    headers: Vec<(&'static str, String)>,
    body: Vec<u8>,
    timeout: Duration,
) -> Result<Reply, ProtoError> {
    let cx = Cx::current().ok_or_else(|| network(&url, "no asupersync context"))?;
    let client = Client::default_for_runtime(&cx);
    let mut request = if post {
        client.post(url.clone())
    } else {
        client.get(url.clone())
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use asupersync::http::h1::server::HostPolicy;
    use asupersync::http::h1::types::{Request, Response};
    use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
    use asupersync::runtime::Runtime;

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
                true,
                format!("http://{addr}/MediaRenderer/AVTransport/Control"),
                vec![("SOAPACTION", "\"urn:x#Play\"".into())],
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
}
