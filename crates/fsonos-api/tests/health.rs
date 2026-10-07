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

#[test]
fn health_over_loopback() {
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
                let _ = server
                    .serve_on_app(&cx, listener, Arc::new(fsonos_api::app()))
                    .await;
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

    assert_eq!(status, 200);
    let health: HealthDto = serde_json::from_slice(&body).expect("JSON body");
    assert_eq!(health.status, "ok");
    assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
}
