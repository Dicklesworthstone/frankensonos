//! Queue removal through the real HTTP server and virtual S1/S2 speakers.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use serde_json::{Value, json};
use std::net::SocketAddr;
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
    server: Arc<TcpServer>,
    worker: Option<JoinHandle<()>>,
}

impl Api {
    fn start(sim: &SimHandle, client: Client) -> Self {
        let mut household = HouseholdState::default();
        household.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("Room").unwrap().ip).unwrap(),
        );
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![household.clone()])),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = Arc::new(fsonos_api::app(
            &Arc::new(surface),
            &Identity::fixed(client),
            &web,
        ));
        let server = Arc::new(TcpServer::new(
            ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec()),
        ));
        let (tx, rx) = mpsc::channel();
        let running = Arc::clone(&server);
        let worker = thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = running.serve_on_app(&cx, listener, app).await;
            });
        });
        Self {
            addr: rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            server,
            worker: Some(worker),
        }
    }

    fn call(&self, path: &str, body: Option<&Value>, headers: &[(&str, &str)]) -> (u16, Value) {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let http = HttpClient::default_for_runtime(&cx);
            let url = format!("http://{}{path}", self.addr);
            let mut request = match body {
                Some(body) => http.post(url).body(body.to_string()),
                None => http.get(url),
            };
            for (name, value) in headers {
                request = request.header(*name, *value);
            }
            let response = request.send(&cx).await.unwrap();
            (
                response.status,
                serde_json::from_slice(&response.body).unwrap(),
            )
        })
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.call(path, None, &[])
    }

    fn remove(&self, body: &Value) -> (u16, Value) {
        self.call(
            "/queue/remove",
            Some(body),
            &[("Content-Type", "application/json")],
        )
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sim(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [SimPlayerSpec::new("Room", model)];
    let builder = SimHousehold::builder();
    let sim = if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap();
    for name in ["first", "second", "third"] {
        soap::enqueue_uri(
            &sim.lan(),
            sim.player("Room").unwrap().ip,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    sim
}

#[test]
fn http_read_remove_and_stale_retry_preserve_unselected_entries_on_s1_and_s2() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let api = Api::start(&sim, Client::LoopbackHttp);
        let (status, before) = api.get("/zones/Room/queue");
        assert_eq!(status, 200, "{before}");
        let request = json!({
            "zone": "Room", "position": 2,
            "expected_uris": [before["items"][1]["uri"]],
            "expected_revision": before["revision"]
        });
        let (status, done) = api.remove(&request);
        assert_eq!(status, 200, "{done}");
        assert_eq!(done["count"], 1);
        assert_eq!(done["remaining"], 2);
        assert_eq!(done["undoable"], false);
        let (status, failed) = api.remove(&request);
        assert_eq!(status, 422, "{failed}");
        assert_eq!(failed["retryable"], false);
        let (_, after) = api.get("/zones/Room/queue");
        assert_eq!(after["total"], 2);
        assert_eq!(after["items"][0]["uri"], before["items"][0]["uri"]);
        assert_eq!(after["items"][1]["uri"], before["items"][2]["uri"]);
        assert_ne!(after["revision"], before["revision"]);
        let (_, spec) = api.get("/openapi.json");
        assert_eq!(
            spec["paths"]["/queue/remove"]["post"]["operationId"],
            "remove_from_queue"
        );
        assert_eq!(
            sim.soap_log()
                .iter()
                .filter(|entry| entry.action == "RemoveTrackRangeFromQueue")
                .count(),
            1
        );
        drop(api);
        sim.shutdown();
    }
}

#[test]
fn removal_inherits_json_and_origin_guards_before_speaker_io() {
    let sim = sim(true);
    let api = Api::start(&sim, Client::LoopbackHttp);
    let body = json!({
        "zone": "Room", "position": 2,
        "expected_uris": ["x-file-cifs://example.invalid/second.flac"]
    });
    let before = sim.soap_log().len();
    let (status, _) = api.call(
        "/queue/remove",
        Some(&body),
        &[("Content-Type", "text/plain")],
    );
    assert_eq!(status, 415);
    let (status, _) = api.call(
        "/queue/remove",
        Some(&body),
        &[
            ("Content-Type", "application/json"),
            ("Origin", "http://untrusted.example.invalid"),
        ],
    );
    assert_eq!(status, 403);
    let (status, _) = api.remove(&json!({ "zone": "Room", "position": 2 }));
    assert_eq!(status, 422);
    assert_eq!(sim.soap_log().len(), before);
    drop(api);
    sim.shutdown();
}

#[test]
fn unidentified_http_clients_cannot_remove_queue_entries() {
    let sim = sim(true);
    let api = Api::start(&sim, Client::Unknown);
    let before = sim.soap_log().len();
    let (status, failure) = api.remove(&json!({
        "zone": "Room", "position": 2,
        "expected_uris": ["x-file-cifs://example.invalid/second.flac"]
    }));
    assert_eq!(status, 403, "{failure}");
    assert_eq!(failure["code"], "POLICY_DENIED");
    assert_eq!(sim.soap_log().len(), before);
    drop(api);
    sim.shutdown();
}
