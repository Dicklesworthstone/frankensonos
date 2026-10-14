//! The public event response bodies on a real fastapi/asupersync listener.
//! No speakers, mocks of the HTTP transport, or external network are used.

use asupersync::Cx;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{Response, ServerConfig, TcpServer};
use fsonos_api::events::{self, EventBus, EventsDto, MAX_EVENT_READERS};
use serde_json::json;
use std::future::ready;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().unwrap())
        .blocking_threads(0, 4)
        .build()
        .unwrap()
}

struct Api {
    addr: SocketAddr,
    bus: Arc<EventBus>,
    server: Arc<TcpServer>,
    thread: Option<JoinHandle<()>>,
    poll_started: mpsc::Receiver<()>,
}

impl Api {
    fn start() -> Self {
        let bus = Arc::new(EventBus::new());
        let live = Arc::clone(&bus);
        let resumed = Arc::clone(&bus);
        let polling = Arc::clone(&bus);
        let immediate = Arc::clone(&bus);
        let future = Arc::clone(&bus);
        let (poll_tx, poll_started) = mpsc::channel();
        let app = fastapi::App::builder()
            .get("/live", move |_, _| {
                ready(events::response(Arc::clone(&live), 0))
            })
            .get("/resumed", move |_, _| {
                ready(events::response(Arc::clone(&resumed), 1))
            })
            .get("/poll", move |_, _| {
                let response =
                    events::poll_response(Arc::clone(&polling), 0, Duration::from_secs(5));
                let _ = poll_tx.send(());
                ready(response)
            })
            .get("/immediate", move |_, _| {
                ready(events::poll_response(
                    Arc::clone(&immediate),
                    0,
                    Duration::ZERO,
                ))
            })
            .get("/future", move |_, _| {
                ready(events::poll_response(
                    Arc::clone(&future),
                    u64::MAX,
                    events::MAX_WAIT,
                ))
            })
            .get("/health", |_, _| {
                ready(Response::json(&json!({ "status": "ok" })).unwrap())
            })
            .build();
        let server = Arc::new(TcpServer::new(ServerConfig::new("127.0.0.1:0")));
        let (addr_tx, addr_rx) = mpsc::channel();
        let serving = Arc::clone(&server);
        let thread = thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                addr_tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = serving.serve_on_app(&cx, listener, Arc::new(app)).await;
            });
        });
        Self {
            addr: addr_rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            bus,
            server,
            thread: Some(thread),
            poll_started,
        }
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        self.bus.publish("topology.changed", json!({}));
        drop(TcpStream::connect(self.addr));
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Http {
    reader: BufReader<TcpStream>,
    status: u16,
    headers: Vec<(String, String)>,
}

impl Http {
    fn get(addr: SocketAddr, path: &str) -> Self {
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(3)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
        let mut headers = Vec::new();
        loop {
            line.clear();
            assert!(reader.read_line(&mut line).unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            let (name, value) = line.split_once(':').unwrap();
            headers.push((name.to_ascii_lowercase(), value.trim().to_string()));
        }
        Self {
            reader,
            status,
            headers,
        }
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    fn chunk(&mut self) -> Option<Vec<u8>> {
        let mut line = String::new();
        assert!(self.reader.read_line(&mut line).unwrap() > 0);
        let size = usize::from_str_radix(line.trim().split(';').next().unwrap(), 16).unwrap();
        assert!(size <= 2 * 1024 * 1024, "fixture chunk unexpectedly large");
        if size == 0 {
            loop {
                line.clear();
                assert!(self.reader.read_line(&mut line).unwrap() > 0);
                if line == "\r\n" {
                    return None;
                }
            }
        }
        let mut bytes = vec![0; size];
        self.reader.read_exact(&mut bytes).unwrap();
        let mut ending = [0; 2];
        self.reader.read_exact(&mut ending).unwrap();
        assert_eq!(&ending, b"\r\n");
        Some(bytes)
    }

    fn body(&mut self) -> Vec<u8> {
        if self.header("transfer-encoding") == Some("chunked") {
            let mut bytes = Vec::new();
            while let Some(chunk) = self.chunk() {
                bytes.extend(chunk);
            }
            bytes
        } else {
            let count: usize = self.header("content-length").unwrap().parse().unwrap();
            assert!(count <= 2 * 1024 * 1024);
            let mut bytes = vec![0; count];
            self.reader.read_exact(&mut bytes).unwrap();
            bytes
        }
    }
}

#[test]
fn a_waiting_poll_does_not_hold_the_http_worker_and_wakes_on_publication() {
    let api = Api::start();
    let addr = api.addr;
    let waiting = thread::spawn(move || {
        let mut reply = Http::get(addr, "/poll");
        assert_eq!(reply.status, 200);
        serde_json::from_slice::<EventsDto>(&reply.body()).unwrap()
    });
    api.poll_started
        .recv_timeout(Duration::from_secs(5))
        .unwrap();
    let mut health = Http::get(api.addr, "/health");
    assert_eq!(health.status, 200);
    assert!(String::from_utf8(health.body()).unwrap().contains("ok"));
    api.bus.publish("zone.state", json!({ "volume": 19 }));
    let response = waiting.join().unwrap();
    assert_eq!(response.last_id, 1);
    assert_eq!(response.events[0].data["volume"], 19);
}

#[test]
fn two_socket_streams_replay_from_independent_cursors() {
    let api = Api::start();
    api.bus.publish("zone.state", json!({ "volume": 10 }));
    api.bus.publish("zone.state", json!({ "volume": 20 }));
    let mut live = Http::get(api.addr, "/live");
    let mut resumed = Http::get(api.addr, "/resumed");
    assert_eq!(live.status, 200);
    assert_eq!(resumed.status, 200);
    assert_eq!(live.header("content-type"), Some("text/event-stream"));
    assert_eq!(live.header("transfer-encoding"), Some("chunked"));
    let mut text = String::new();
    while !text.contains("id: 2\n") {
        text.push_str(&String::from_utf8(live.chunk().unwrap()).unwrap());
    }
    assert!(text.find("id: 1\n").unwrap() < text.find("id: 2\n").unwrap());
    let resumed_text = String::from_utf8(resumed.chunk().unwrap()).unwrap();
    assert!(resumed_text.contains("id: 2\n"));
    assert!(!resumed_text.contains("id: 1\n"));
    api.bus.publish("zone.state", json!({ "volume": 30 }));
    assert!(
        String::from_utf8(live.chunk().unwrap())
            .unwrap()
            .contains("id: 3\n")
    );
    assert!(
        String::from_utf8(resumed.chunk().unwrap())
            .unwrap()
            .contains("id: 3\n")
    );
}

#[test]
fn full_event_admission_returns_503_but_health_and_immediate_reads_still_work() {
    let api = Api::start();
    let mut readers: Vec<_> = (0..MAX_EVENT_READERS)
        .map(|_| events::stream(Arc::clone(&api.bus), 0))
        .collect();
    for path in ["/live", "/poll"] {
        let mut refused = Http::get(api.addr, path);
        assert_eq!(refused.status, 503);
        assert_eq!(refused.header("retry-after"), Some("1"));
        let body: serde_json::Value = serde_json::from_slice(&refused.body()).unwrap();
        assert_eq!(body["code"], "NOT_READY");
    }
    let mut health = Http::get(api.addr, "/health");
    assert_eq!(health.status, 200);
    let _ = health.body();
    let mut immediate = Http::get(api.addr, "/immediate");
    assert_eq!(immediate.status, 200);
    assert!(
        serde_json::from_slice::<EventsDto>(&immediate.body())
            .unwrap()
            .events
            .is_empty()
    );
    drop(readers.pop());
    api.bus.publish("zone.state", json!({}));
    let mut admitted = Http::get(api.addr, "/poll");
    assert_eq!(admitted.status, 200);
    assert_eq!(
        serde_json::from_slice::<EventsDto>(&admitted.body())
            .unwrap()
            .last_id,
        1
    );
}

#[test]
fn a_maximum_peer_cursor_returns_a_json_reset_instead_of_panicking() {
    let api = Api::start();
    let mut response = Http::get(api.addr, "/future");
    assert_eq!(response.status, 200);
    let reset: EventsDto = serde_json::from_slice(&response.body()).unwrap();
    assert_eq!(reset.reset, Some(1));
    assert_eq!(reset.last_id, 0);
    assert!(reset.events.is_empty());
}
