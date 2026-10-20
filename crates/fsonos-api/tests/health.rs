//! FND-DEPS proof: the fastapi app serves `GET /health` on a real loopback
//! socket, fetched with the asupersync HTTP client (one shared runtime line).

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::HealthDto;
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

/// The app over a surface with no speakers (`/health` needs none), following
/// `data_dir` when given, as the daemon's does.
fn app(data_dir: Option<&std::path::Path>) -> fastapi::App {
    struct NoLan;
    impl fsonos_proto::Transport for NoLan {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            _: &str,
            _: &str,
        ) -> Result<String, fsonos_proto::ProtoError> {
            Err(fsonos_proto::ProtoError::NotWired("no LAN in this test"))
        }
    }
    let surface = fsonos_api::Surface::new(
        Box::new(NoLan),
        Box::new(|_| Ok(Vec::new())),
        fsonos_core::policy::Policy::default(),
        Box::new(fsonos_core::clock::SystemClock),
    );
    let surface = match data_dir {
        Some(dir) => surface.with_policy_file(dir.to_path_buf()),
        None => surface,
    };
    fsonos_api::app(
        &Arc::new(surface),
        &fsonos_api::Identity::fixed(fsonos_core::policy::Client::LoopbackHttp),
        &fsonos_api::WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]),
    )
}

/// `GET /health` from `app`, served once on loopback: its status and body.
fn get_health(app: fastapi::App) -> (u16, HealthDto) {
    let server = Arc::new(TcpServer::new(ServerConfig::new("127.0.0.1:0")));
    let (addr_tx, addr_rx) = mpsc::channel();
    let server_thread = {
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
                let _ = server.serve_on_app(&cx, listener, Arc::new(app)).await;
            });
        })
    };
    let addr = addr_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("server ready");

    let (status, body) = runtime().block_on(async move {
        let cx = Cx::current().expect("ambient Cx");
        let resp = Client::default_for_runtime(&cx)
            .get(format!("http://{addr}/health"))
            .send(&cx)
            .await
            .expect("GET /health");
        (resp.status, resp.body)
    });

    server.shutdown();
    // Wake the accept loop so it observes shutdown.
    drop(std::net::TcpStream::connect(addr));
    server_thread.join().expect("server thread");

    (status, serde_json::from_slice(&body).expect("JSON body"))
}

#[test]
fn health_over_loopback() {
    let (status, health) = get_health(app(None));
    assert_eq!(status, 200);
    assert_eq!(health.status, "ok");
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    assert_eq!(health.selftest, None);
}

/// The daemon's health names its last self-test: when, and whether it
/// passed (sbj9.4), and nothing more.
#[test]
fn health_names_the_last_self_test() {
    let dir = tempfile::tempdir().unwrap();
    let (_, health) = get_health(app(Some(dir.path())));
    assert_eq!(health.selftest, None, "no run kept yet");
    let kept = r#"{"at": 1700000000, "version": "x", "report": {"exit_code": 6, "checks": []}}"#;
    std::fs::write(dir.path().join(fsonos_core::selftest::LAST_FILE), kept).unwrap();
    let (_, health) = get_health(app(Some(dir.path())));
    assert_eq!(
        health.selftest,
        Some(fsonos_api::SelftestHealth {
            at: 1_700_000_000,
            passed: false
        })
    );
}
