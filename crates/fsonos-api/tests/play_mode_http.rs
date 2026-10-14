//! Mode control through the actual HTTP router and virtual S1/S2 speakers.

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
use fsonos_types::PlayerId;
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
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("Room").unwrap().ip).unwrap(),
        );
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(move |_| Ok(vec![house.clone()])),
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
        let running = Arc::clone(&server);
        let (tx, rx) = mpsc::channel();
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

    fn set(&self, body: &Value) -> (u16, Value) {
        self.call(
            "/play-mode",
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
    let room = sim.player("Room").unwrap();
    let lan = sim.lan();
    for name in ["first", "second"] {
        soap::enqueue_uri(
            &lan,
            room.ip,
            &format!("x-file-cifs://example.invalid/{name}.flac"),
            "",
            0,
            false,
        )
        .unwrap();
    }
    soap::play_from_queue(&lan, room.ip, &PlayerId(room.uuid.clone())).unwrap();
    soap::play(&lan, room.ip).unwrap();
    sim.advance(Duration::from_secs(9));
    sim
}

#[test]
fn http_reads_changes_and_stale_preconditions_preserve_playback_on_both_generations() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let api = Api::start(&sim, Client::LoopbackHttp);
        let host = sim.player("Room").unwrap().ip;
        let position = soap::get_position_info(&sim.lan(), host).unwrap();
        let (status, read) = api.call("/zones/Room/play-mode", None, &[]);
        assert_eq!(status, 200, "{read}");
        assert_eq!(read["mode"], "NORMAL");
        assert_eq!(read["queue_active"], true);
        assert_eq!(read["dj_active"], false);
        let request =
            json!({ "zone": "Room", "mode": "SHUFFLE_REPEAT_ONE", "expected_mode": "NORMAL" });
        let (status, changed) = api.set(&request);
        assert_eq!(status, 200, "{changed}");
        assert_eq!(changed["state"]["mode"], "SHUFFLE_REPEAT_ONE");
        assert_eq!(changed["changed"], true);
        let (status, stale) = api.set(&request);
        assert_eq!(status, 422, "{stale}");
        assert_eq!(stale["retryable"], false);
        assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap(), position);
        assert_eq!(
            sim.soap_log()
                .iter()
                .filter(|call| call.action == "SetPlayMode")
                .count(),
            1
        );
        let (status, spec) = api.call("/openapi.json", None, &[]);
        assert_eq!(status, 200);
        assert_eq!(
            spec["paths"]["/zones/{room}/play-mode"]["get"]["operationId"],
            "get_play_mode"
        );
        assert_eq!(
            spec["paths"]["/play-mode"]["post"]["operationId"],
            "set_play_mode"
        );
        drop(api);
        sim.shutdown();
    }
}

#[test]
fn mode_routes_inherit_json_origin_and_body_guards_before_speaker_io() {
    let sim = sim(true);
    let api = Api::start(&sim, Client::LoopbackHttp);
    let count = sim.soap_log().len();
    let body = json!({ "zone": "Room", "mode": "SHUFFLE" });
    assert_eq!(
        api.call("/play-mode", Some(&body), &[("Content-Type", "text/plain")])
            .0,
        415
    );
    assert_eq!(
        api.call(
            "/play-mode",
            Some(&body),
            &[
                ("Content-Type", "application/json"),
                ("Origin", "http://untrusted.example.invalid"),
            ]
        )
        .0,
        403
    );
    for invalid in [
        json!({ "zone": "Room" }),
        json!({ "zone": "Room", "mode": "INVALID" }),
        json!({ "zone": "Room", "mode": "NORMAL", "expect_mode": "NORMAL" }),
    ] {
        assert_eq!(api.set(&invalid).0, 422);
    }
    assert_eq!(sim.soap_log().len(), count);
    drop(api);
    sim.shutdown();
}

#[test]
fn unidentified_http_clients_may_read_but_cannot_change_the_mode() {
    let sim = sim(false);
    let api = Api::start(&sim, Client::Unknown);
    assert_eq!(api.call("/zones/Room/play-mode", None, &[]).0, 200);
    let count = sim.soap_log().len();
    let (status, error) = api.set(&json!({ "zone": "Room", "mode": "REPEAT_ALL" }));
    assert_eq!(status, 403, "{error}");
    assert_eq!(error["code"], "POLICY_DENIED");
    assert_eq!(sim.soap_log().len(), count);
    drop(api);
    sim.shutdown();
}
