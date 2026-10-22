//! Queue search through the HTTP listener and virtual S1/S2 speakers.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::surface::queue::{QueueSearchDto, QueueSearchRequest};
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
    fn start(sim: &SimHandle, policy: Policy) -> Self {
        let mut household = HouseholdState::default();
        household.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("Room").unwrap().ip).unwrap(),
        );
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![household.clone()])),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(MemStore::default()), "http");
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = Arc::new(fsonos_api::app(
            &Arc::new(surface),
            &Identity::fixed(Client::LoopbackHttp),
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

fn sim(s2: bool, populated: bool) -> SimHandle {
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
    if populated {
        let host = sim.player("Room").unwrap().ip;
        for position in 1..=105 {
            let matched = matches!(position, 2 | 101 | 105);
            let title = if matched {
                "Prélude in G"
            } else {
                "Other music"
            };
            let metadata = format!(
                "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
                 xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">\
                 <item id=\"test:{position}\"><dc:title>{title}</dc:title>\
                 <dc:creator>Dvořák</dc:creator><upnp:album>Concert &amp; Stage</upnp:album>\
                 <upnp:class>object.item.audioItem.musicTrack</upnp:class></item></DIDL-Lite>"
            );
            // Matching occurrences deliberately render the same URI.
            let uri = if matched {
                "x-file-cifs://example.invalid/prelude.flac".into()
            } else {
                format!("x-file-cifs://example.invalid/other-{position}.flac")
            };
            soap::enqueue_uri(&sim.lan(), host, &uri, &metadata, 0, false).unwrap();
        }
    }
    sim
}

fn request() -> QueueSearchRequest {
    QueueSearchRequest {
        zone: "Room".into(),
        query: "DVORAK prelude concert".into(),
        offset: 0,
        limit: 2,
        expected_revision: None,
    }
}

#[test]
fn http_search_continues_across_pages_and_plays_an_exact_duplicate_on_both_generations() {
    for s2 in [false, true] {
        let sim = sim(s2, true);
        let api = Api::start(&sim, Policy::default());
        let before = sim.soap_log().len();
        let mut query = request();
        let (status, body) = api.get(&query.path().unwrap());
        assert_eq!(status, 200, "{body}");
        let first: QueueSearchDto = serde_json::from_value(body).unwrap();
        assert_eq!(first.total, 105);
        assert_eq!(
            first.items.iter().map(|i| i.position).collect::<Vec<_>>(),
            [2, 101]
        );
        assert_eq!(first.items[0].uri, first.items[1].uri);
        assert_eq!(
            (first.offset, first.examined, first.next_offset),
            (0, 101, Some(101))
        );
        assert!(!first.complete);

        query.offset = first.next_offset.unwrap();
        query.expected_revision = Some(first.revision.clone());
        let (status, body) = api.get(&query.path().unwrap());
        assert_eq!(status, 200, "{body}");
        let last: QueueSearchDto = serde_json::from_value(body).unwrap();
        assert_eq!(last.items.len(), 1);
        assert_eq!(last.items[0].position, 105);
        assert_eq!(last.revision, first.revision);
        assert_eq!(last.examined, 4);
        assert!(last.complete);
        assert!(last.next_offset.is_none());
        assert!(
            sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
        );

        let (status, result) = api.call(
            "/queue/play",
            Some(&json!({
                "zone": "Room", "position": first.items[1].position,
                "expected_uri": first.items[1].uri,
                "expected_revision": first.revision,
            })),
            &[("Content-Type", "application/json")],
        );
        assert_eq!(status, 200, "{result}");
        assert_eq!(
            soap::get_position_info(&sim.lan(), sim.player("Room").unwrap().ip)
                .unwrap()
                .track,
            101,
        );

        let (_, spec) = api.get("/openapi.json");
        let operation = &spec["paths"]["/zones/{room}/queue/search"]["get"];
        assert_eq!(operation["operationId"], "search_queue");
        let parameters = operation["parameters"].as_array().unwrap();
        assert!(
            parameters
                .iter()
                .any(|p| p["name"] == "q" && p["required"] == true)
        );
        assert!(parameters.iter().any(|p| p["name"] == "expected_revision"));
    }
}

#[test]
fn a_changed_queue_rejects_search_continuation_without_mutating_playback() {
    let sim = sim(true, true);
    let api = Api::start(&sim, Policy::default());
    let mut query = request();
    let (status, body) = api.get(&query.path().unwrap());
    assert_eq!(status, 200, "{body}");
    let first: QueueSearchDto = serde_json::from_value(body).unwrap();
    soap::enqueue_uri(
        &sim.lan(),
        sim.player("Room").unwrap().ip,
        "x-file-cifs://example.invalid/another.flac",
        "",
        0,
        false,
    )
    .unwrap();
    query.offset = first.next_offset.unwrap();
    query.expected_revision = Some(first.revision);
    let before = sim.soap_log().len();
    let (status, failure) = api.get(&query.path().unwrap());
    assert_eq!(status, 409, "{failure}");
    assert_eq!(failure["code"], "QUEUE_CHANGED");
    assert!(
        sim.soap_log()[before..]
            .iter()
            .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
    );
}

#[test]
fn malformed_searches_and_untrusted_origins_do_not_reach_speakers() {
    let sim = sim(true, false);
    let api = Api::start(&sim, Policy::default());
    let before = sim.soap_log().len();
    for suffix in [
        "",
        "?q=",
        "?q=music&limit=0",
        "?q=music&limit=201",
        "?q=music&offset=1",
        "?q=music&offset=-1",
        "?q=music&offset=4294967296",
        "?q=music&limit=many",
        "?q=%FF",
        "?q=first&q=second",
        "?q=music&offset=0&offset=1",
        "?q=music&unknown=1",
    ] {
        let (status, failure) = api.get(&format!("/zones/Room/queue/search{suffix}"));
        assert_eq!(status, 422, "{suffix}: {failure}");
    }
    let (status, _) = api.call(
        "/zones/Room/queue/search?q=music",
        None,
        &[("Origin", "https://untrusted.example.invalid")],
    );
    assert_eq!(status, 403);
    assert_eq!(sim.soap_log().len(), before);
}

#[test]
fn queue_read_policy_also_controls_search_and_empty_results_are_complete() {
    let sim = sim(true, false);
    let policy = Policy::from_toml("[clients.loopback-http]\ndeny = ['get_queue']").unwrap();
    let denied = Api::start(&sim, policy);
    let before = sim.soap_log().len();
    let (status, failure) = denied.get("/zones/Room/queue/search?q=music");
    assert_eq!(status, 403, "{failure}");
    assert_eq!(failure["code"], "POLICY_DENIED");
    assert_eq!(sim.soap_log().len(), before);
    drop(denied);

    let api = Api::start(&sim, Policy::default());
    let (status, body) = api.get("/zones/Room/queue/search?q=music");
    assert_eq!(status, 200, "{body}");
    let empty: QueueSearchDto = serde_json::from_value(body).unwrap();
    assert!(empty.complete);
    assert!(empty.items.is_empty());
    assert!(empty.next_offset.is_none());
    assert_eq!((empty.total, empty.examined), (0, 0));
}
