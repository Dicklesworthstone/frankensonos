//! The legacy bridge's API (vg3s.9) over real loopback HTTP and virtual
//! speakers: its routes answer in its shapes, and its writes run as the
//! `bridge-compat` client, held to the house caps, logged and undoable.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::{Surface, WebPolicy};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
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

struct Bridge {
    addr: SocketAddr,
    surface: Arc<Surface>,
    server: Arc<TcpServer>,
    worker: Option<JoinHandle<()>>,
}

impl Bridge {
    /// The bridge's API over `sim`, under `policy`.
    fn start(sim: &SimHandle, policy: Policy) -> Self {
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(&sim.lan(), sim.player("Kitchen").unwrap().ip).unwrap(),
        );
        let surface = Arc::new(
            Surface::new(
                Box::new(sim.lan()),
                Box::new(move |_| Ok(vec![house.clone()])),
                policy,
                Box::new(SystemClock),
            )
            .with_action_log(Box::new(MemStore::default()), "bridge"),
        );
        let app = Arc::new(fsonos_api::http::bridge::app(&surface));
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
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
            surface,
            server,
            worker: Some(worker),
        }
    }

    /// `path` (a POST when there is a `body`), with `headers`: the status and
    /// the JSON answer.
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
            let body = serde_json::from_slice(&response.body).unwrap_or(Value::Null);
            (response.status, body)
        })
    }

    fn get(&self, path: &str) -> (u16, Value) {
        self.call(path, None, &[])
    }

    /// A POST as the bridge's callers sent it: JSON, no browser headers.
    fn post(&self, path: &str, body: &Value) -> (u16, Value) {
        self.call(path, Some(body), &[("Content-Type", "application/json")])
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .spawn()
        .unwrap()
}

#[test]
fn the_bridge_reads_answer_in_its_shapes() {
    let sim = sim();
    let bridge = Bridge::start(&sim, Policy::default());

    let (status, health) = bridge.get("/health");
    assert_eq!(
        (status, &health),
        (200, &json!({ "ok": true, "devices": 2 }))
    );

    let (status, zones) = bridge.get("/zones");
    assert_eq!(status, 200, "{zones}");
    let zones = zones["zones"].as_array().unwrap();
    assert_eq!(zones.len(), 2, "{zones:?}");
    let kitchen = zones.iter().find(|z| z["name"] == "Kitchen").unwrap();
    assert_eq!(kitchen["ip"], sim.player("Kitchen").unwrap().ip.to_string());
    assert!(kitchen["model"].is_string(), "{kitchen}");
    assert_eq!(kitchen["is_coordinator"], true);
    assert_eq!(kitchen["coordinator"], "Kitchen");
    assert_eq!(kitchen["group_members"], json!(["Kitchen"]));
    assert_eq!(kitchen["transport_state"], "STOPPED");

    let (status, now) = bridge.get("/status?zone=Kitchen");
    assert_eq!(status, 200, "{now}");
    for key in ["title", "artist", "album"] {
        assert!(now[key].is_string(), "{key}: {now}");
    }
    assert_eq!(now["zone"], "Kitchen");
    assert_eq!(now["transport_state"], "STOPPED");
    assert_eq!(now["position"], "0:00:00");
    assert_eq!(now["volume"], 20);
    assert_eq!(now["muted"], false);

    let (status, favorites) = bridge.get("/favorites?zone=Kitchen");
    assert_eq!(status, 200, "{favorites}");
    assert_eq!(favorites["zone"], "Kitchen");
    assert!(favorites["favorites"].is_array(), "{favorites}");

    let (status, media) = bridge.get("/mediainfo?zone=Kitchen");
    assert_eq!(status, 200, "{media}");
    assert_eq!(media["zone"], "Kitchen");
    assert!(media["current_uri"].is_string(), "{media}");
    assert!(media["current_uri_metadata"].is_string(), "{media}");

    // An unknown zone answers as the bridge did, with the rooms there are.
    let (status, unknown) = bridge.get("/status?zone=Attic");
    assert_eq!(status, 404, "{unknown}");
    assert_eq!(
        unknown,
        json!({ "error": "no such zone", "zone": "Attic", "known": ["Kitchen", "Office"] })
    );

    let (status, again) = bridge.post("/rediscover", &json!({}));
    assert_eq!(
        (status, &again),
        (200, &json!({ "ok": true, "devices": 2 }))
    );
}

#[test]
fn the_bridge_writes_run_under_the_house_policy_logged_and_undoable() {
    let sim = sim();
    let policy = Policy::from_toml("[defaults]\nmax_volume = 30\n").unwrap();
    let bridge = Bridge::start(&sim, policy);
    let kitchen = sim.player("Kitchen").unwrap().ip;

    // The bridge set whatever level it was asked; the caps hold it now.
    let (status, set) = bridge.post("/volume", &json!({ "zone": "kitchen", "level": 80 }));
    assert_eq!(status, 200, "{set}");
    assert_eq!(set, json!({ "ok": true, "zone": "Kitchen", "volume": 30 }));
    assert_eq!(soap::get_volume(&sim.lan(), kitchen).unwrap(), 30);

    let (status, muted) = bridge.post("/mute", &json!({ "zone": "Kitchen" }));
    assert_eq!(
        (status, &muted),
        (
            200,
            &json!({ "ok": true, "zone": "Kitchen", "muted": true })
        )
    );
    assert!(soap::get_mute(&sim.lan(), kitchen).unwrap());

    // Both are the bridge-compat client's, in the log, and it can undo them:
    // the mute, then the level.
    let log = |bridge: &Bridge| {
        bridge
            .surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
    };
    let written = log(&bridge);
    assert_eq!(written.len(), 2, "{written:?}");
    assert!(
        written.iter().all(|a| a.action.client == "bridge-compat"),
        "{written:?}"
    );
    for _ in 0..2 {
        bridge
            .surface
            .undo(&Client::BridgeCompat, true)
            .unwrap()
            .expect("an action to undo");
    }
    assert_eq!(soap::get_volume(&sim.lan(), kitchen).unwrap(), 20);
    assert!(!soap::get_mute(&sim.lan(), kitchen).unwrap());

    let (status, played) = bridge.post(
        "/play",
        &json!({ "zone": "Kitchen", "uri": "x-rincon-mp3radio://radio.example.com/live" }),
    );
    assert_eq!(status, 200, "{played}");
    assert_eq!(played, json!({ "ok": true, "played_on": "Kitchen" }));
    let (status, paused) = bridge.post("/pause", &json!({ "zone": "Kitchen" }));
    assert_eq!(
        (status, &paused),
        (200, &json!({ "ok": true, "zone": "Kitchen" }))
    );
    let written = log(&bridge);
    let plays = written
        .iter()
        .filter(|a| a.action.intent.contains("play") || a.action.intent.contains("pause"))
        .count();
    assert!(plays >= 2, "{written:?}");
    assert!(
        written.iter().all(|a| a.action.client == "bridge-compat"),
        "{written:?}"
    );
}

#[test]
fn a_web_page_cannot_drive_the_bridge() {
    let sim = sim();
    let bridge = Bridge::start(&sim, Policy::default());
    for headers in [
        vec![
            ("Origin", "https://evil.example"),
            ("Content-Type", "text/plain"),
        ],
        vec![("Sec-Fetch-Site", "cross-site")],
    ] {
        let (status, refused) = bridge.call(
            "/volume",
            Some(&json!({ "zone": "Kitchen", "level": 90 })),
            &headers,
        );
        assert_eq!(status, 403, "{refused}");
    }
    let kitchen = sim.player("Kitchen").unwrap().ip;
    assert_eq!(soap::get_volume(&sim.lan(), kitchen).unwrap(), 20);
    let (status, _) = bridge.call("/health", None, &[("Origin", "https://evil.example")]);
    assert_eq!(status, 403);
}
