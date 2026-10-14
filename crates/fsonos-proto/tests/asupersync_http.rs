//! FND-DEPS proof: the asupersync HTTP/1.1 client (SOAP control, Spotify) can
//! GET from an asupersync `Http1Listener` (the GENA callback sink) over a real
//! localhost socket. No network beyond loopback.
//!
//! asupersync 0.5's h1 server is secure by default: `HostPolicy::RejectUnknown`
//! answers every request 421 Misdirected Request. A listener must name the
//! Host values it serves — for the GENA sink, the LAN address the players are
//! given in the SUBSCRIBE `CALLBACK` URL.

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::{Request, Response};
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::mpsc;
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
fn client_gets_from_local_http1_listener() {
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
                |req: Request| async move {
                    Response::new(200, "OK", format!("pong {}", req.uri).into_bytes())
                        .with_header("Content-Type", "text/plain")
                },
                config,
            )
            .await
            .expect("bind loopback listener");
            let addr = listener.local_addr().expect("local addr");
            ready_tx
                .send((addr, listener.shutdown_signal()))
                .expect("report addr");
            listener.run(&handle).await.expect("listener run");
        });
    });

    let (addr, shutdown) = ready_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("listener ready");

    let (status, body) = runtime().block_on(async move {
        let cx = Cx::current().expect("runtime installs an ambient Cx");
        let client = Client::default_for_runtime(&cx);
        let resp = client
            .get(format!("http://{addr}/ping?x=1"))
            .send(&cx)
            .await
            .expect("GET over loopback");
        (resp.status, resp.body)
    });

    // The client derives Host from the URL, so send a foreign Host raw.
    let mut raw = TcpStream::connect(addr).expect("connect");
    raw.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    raw.write_all(b"GET /ping HTTP/1.1\r\nHost: not-listed.example\r\nConnection: close\r\n\r\n")
        .expect("write raw request");
    let mut reply = String::new();
    raw.read_to_string(&mut reply).expect("read raw reply");

    shutdown.trigger_immediate();
    server.join().expect("server thread");

    assert_eq!(status, 200);
    assert_eq!(String::from_utf8(body).unwrap(), "pong /ping?x=1");
    assert!(
        reply.starts_with("HTTP/1.1 421"),
        "a Host outside the allow-list is refused: {reply}"
    );
}
