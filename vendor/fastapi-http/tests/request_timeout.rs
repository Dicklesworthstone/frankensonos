//! Request-timeout enforcement tests (mea-ym5r5).
//!
//! The server must answer 504 at the configured request deadline by racing the
//! handler future against the deadline — not by letting the handler run to
//! completion and substituting 504 afterwards. A handler that never completes
//! must still produce a 504 at the deadline, the abandoned request's
//! connection must close, and in-budget handlers must be untouched. The server
//! must also publish the deadline on `RequestContext` so middleware with
//! externally visible side effects can refuse to publish abandoned responses.

use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use asupersync::{Cx, Time};
use fastapi_core::{App, FromRequest, Request, RequestContext, Response, ResponseBody, StatusCode};
use fastapi_http::{ServerConfig, TcpServer};
use std::io::Read;
use std::io::Write as _;
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

#[test]
fn handler_deadline_releases_registered_dependency() {
    let released = Arc::new(AtomicBool::new(false));
    let cleanup_released = Arc::clone(&released);
    let app = App::builder()
        .get(
            "/stuck-resource",
            move |ctx: &RequestContext, _req: &mut Request| {
                let cleanup_released = Arc::clone(&cleanup_released);
                ctx.cleanup_stack().push(Box::new(move || {
                    Box::pin(async move {
                        cleanup_released.store(true, Ordering::SeqCst);
                    })
                }));
                std::future::pending::<Response>()
            },
        )
        .build();
    let config = ServerConfig::new("127.0.0.1:0").with_request_timeout(Time::from_millis(300));
    let (server, addr, server_thread) = spawn_app_server(app, config);
    let response = get_response(addr, "/stuck-resource");
    assert!(response.starts_with(b"HTTP/1.1 504"));
    assert!(
        String::from_utf8_lossy(&response)
            .to_ascii_lowercase()
            .contains("connection: close")
    );
    assert!(
        released.load(Ordering::SeqCst),
        "deadline must finalize the retained request context"
    );
    server.shutdown();
    drop(TcpStream::connect(addr));
    server_thread.join().expect("deadline server join");
}

#[test]
fn http1_stream_and_background_use_resource_before_cleanup() {
    for handler_mode in [false, true] {
        let events = Arc::new(std::sync::Mutex::new(Vec::<&'static str>::new()));
        let handler_events = Arc::clone(&events);
        let app = App::builder()
            .state(Arc::clone(&handler_events))
            .route_entry(fastapi_core::app::RouteEntry::new_boxed(
                fastapi_core::Method::Get,
                "/resource",
                |ctx: &RequestContext, req: &mut Request| {
                    Box::pin(async move {
                        let fastapi_core::State(handler_events) = fastapi_core::State::<
                            Arc<std::sync::Mutex<Vec<&'static str>>>,
                        >::from_request(
                            ctx, req
                        )
                        .await
                        .expect("configured state on TCP request");
                        let released = Arc::new(AtomicBool::new(false));
                        let cleanup_released = Arc::clone(&released);
                        let cleanup_events = Arc::clone(&handler_events);
                        ctx.cleanup_stack().push(Box::new(move || {
                            Box::pin(async move {
                                cleanup_released.store(true, Ordering::SeqCst);
                                cleanup_events.lock().expect("events").push("cleanup");
                            })
                        }));
                        let background_released = Arc::clone(&released);
                        let background_events = Arc::clone(&handler_events);
                        let tasks = fastapi_core::BackgroundTasks::new();
                        tasks.add(move || {
                            assert!(!background_released.load(Ordering::SeqCst));
                            background_events.lock().expect("events").push("background");
                        });
                        req.insert_extension(tasks);
                        let body_events = Arc::clone(&handler_events);
                        let body = asupersync::stream::iter(std::iter::once_with(move || {
                            assert!(!released.load(Ordering::SeqCst));
                            body_events.lock().expect("events").push("body");
                            b"resource live".to_vec()
                        }));
                        Response::ok().body(ResponseBody::stream(body))
                    })
                },
            ))
            .build();
        let (server, addr, server_thread) = spawn_cleanup_server(app, handler_mode);
        let response = get_response(addr, "/resource");
        assert!(response.starts_with(b"HTTP/1.1 200"));
        assert!(String::from_utf8_lossy(&response).contains("resource live"));
        assert!(
            response.ends_with(b"0\r\n\r\n"),
            "chunked response must finish"
        );
        assert_eq!(
            *events.lock().expect("events"),
            ["body", "background", "cleanup"]
        );
        server.shutdown();
        drop(TcpStream::connect(addr));
        server_thread.join().expect("streaming server join");
    }
}

fn spawn_cleanup_server(
    app: App,
    handler_mode: bool,
) -> (Arc<TcpServer>, SocketAddr, std::thread::JoinHandle<()>) {
    if !handler_mode {
        return spawn_app_server(app, ServerConfig::new("127.0.0.1:0"));
    }
    let server = Arc::new(TcpServer::new(ServerConfig::new("127.0.0.1:0")));
    let (addr_tx, addr_rx) = mpsc::channel();
    let thread_server = Arc::clone(&server);
    let server_thread = std::thread::spawn(move || {
        let reactor = create_reactor().expect("reactor");
        let rt = RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let cx = Cx::current().expect("runtime Cx");
            let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            addr_tx
                .send(listener.local_addr().expect("address"))
                .expect("send address");
            let handler: Arc<dyn fastapi_core::Handler> = Arc::new(app);
            let result = thread_server.serve_on_handler(&cx, listener, handler).await;
            assert!(
                matches!(result, Err(fastapi_http::ServerError::Shutdown)),
                "explicit server drain must return Shutdown: {result:?}"
            );
        });
    });
    (
        server,
        addr_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("server address"),
        server_thread,
    )
}

#[test]
fn response_write_error_still_cleans_up_and_skips_background_tasks() {
    let released = Arc::new(AtomicBool::new(false));
    let background_ran = Arc::new(AtomicBool::new(false));
    let (addr_tx, addr_rx) = mpsc::channel();
    let (result_tx, result_rx) = mpsc::channel();
    let cleanup_released = Arc::clone(&released);
    let task_ran = Arc::clone(&background_ran);
    let server_thread = std::thread::spawn(move || {
        let reactor = create_reactor().expect("reactor");
        let rt = RuntimeBuilder::current_thread()
            .with_reactor(reactor)
            .build()
            .expect("runtime");
        rt.block_on(async move {
            let cx = Cx::current().expect("runtime Cx");
            let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind");
            addr_tx
                .send(listener.local_addr().expect("address"))
                .expect("send address");
            let (mut stream, peer) = listener.accept().await.expect("accept");
            // A real socket with its write half closed deterministically fails the response write.
            stream
                .shutdown(std::net::Shutdown::Write)
                .expect("close write half");
            let expected_error = fastapi_http::write_all(&mut stream, b"closed-write probe")
                .await
                .expect_err("real socket write must fail")
                .kind();
            let config = ServerConfig::new("127.0.0.1:0");
            let result = fastapi_http::process_connection(
                &cx,
                &AtomicU64::new(0),
                stream,
                peer,
                &config,
                move |ctx, req| {
                    let cleanup_released = Arc::clone(&cleanup_released);
                    ctx.cleanup_stack().push(Box::new(move || {
                        Box::pin(async move {
                            cleanup_released.store(true, Ordering::SeqCst);
                        })
                    }));
                    let task_ran = Arc::clone(&task_ran);
                    let tasks = fastapi_core::BackgroundTasks::new();
                    tasks.add(move || task_ran.store(true, Ordering::SeqCst));
                    req.insert_extension(tasks);
                    std::future::ready(
                        Response::ok().body(ResponseBody::Bytes(b"write must fail".to_vec())),
                    )
                },
            )
            .await;
            result_tx
                .send((result, expected_error))
                .expect("send write result");
        });
    });
    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server address");
    let mut client = TcpStream::connect(addr).expect("connect");
    client
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .expect("request");
    let (result, expected_error) = result_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("write result");
    match result {
        Err(fastapi_http::ServerError::Io(err)) => {
            assert_eq!(err.kind(), expected_error);
        }
        result => panic!("original socket write error must survive teardown: {result:?}"),
    }
    assert!(released.load(Ordering::SeqCst));
    assert!(!background_ran.load(Ordering::SeqCst));
    server_thread.join().expect("write-error server join");
}

fn spawn_app_server(
    app: App,
    config: ServerConfig,
) -> (Arc<TcpServer>, SocketAddr, std::thread::JoinHandle<()>) {
    let server = Arc::new(TcpServer::new(config));
    let app = Arc::new(app);
    let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();

    let server_thread = {
        let server = Arc::clone(&server);
        let app = Arc::clone(&app);
        std::thread::spawn(move || {
            let reactor = create_reactor().expect("test reactor must build");
            let rt = RuntimeBuilder::current_thread()
                .with_reactor(reactor)
                .build()
                .expect("test runtime must build");
            rt.block_on(async move {
                let cx = Cx::current().expect("test runtime must install an ambient Cx");
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind must succeed");
                let local_addr = listener.local_addr().expect("local_addr must work");
                addr_tx.send(local_addr).expect("addr send must succeed");
                let _ = server.serve_on_app(&cx, listener, app).await;
            });
        })
    };

    let addr = addr_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("server must report addr");
    (server, addr, server_thread)
}

fn get_response(addr: SocketAddr, path: &str) -> Vec<u8> {
    let mut stream = TcpStream::connect(addr).expect("connect must succeed");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .expect("set_read_timeout must succeed");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .expect("request write must succeed");

    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(err) => panic!("response read must not time out or fail: {err}"),
        }
    }
    response
}

/// A handler that never completes must be abandoned at the request deadline
/// and answered with 504 — under the pre-fix behavior (post-hoc elapsed
/// check after awaiting the handler to completion) this request would hang
/// until the client read timeout instead.
#[test]
fn stuck_handler_gets_504_at_deadline_and_connection_closes() {
    let app = App::builder()
        .get("/stuck", |_ctx: &RequestContext, _req: &mut Request| {
            std::future::pending::<Response>()
        })
        .build();
    let config = ServerConfig::new("127.0.0.1:0").with_request_timeout(Time::from_millis(300));
    let (_server, addr, _server_thread) = spawn_app_server(app, config);

    let started = Instant::now();
    let response = get_response(addr, "/stuck");
    let elapsed = started.elapsed();

    let head = String::from_utf8_lossy(&response);
    assert!(
        head.starts_with("HTTP/1.1 504"),
        "stuck handler must be answered with 504 at the deadline, got: {head}"
    );
    assert!(
        head.to_ascii_lowercase().contains("connection: close"),
        "an abandoned request must close its connection, got: {head}"
    );
    assert!(
        elapsed >= Duration::from_millis(200),
        "504 must come from the deadline race, not an instant failure ({elapsed:?})"
    );
}

/// An in-budget handler is delivered unchanged, and it observes the request
/// deadline via `RequestContext` (set, on the runtime clock, not yet
/// exceeded).
#[test]
fn fast_handler_unaffected_and_observes_deadline() {
    let app = App::builder()
        .get("/fast", |ctx: &RequestContext, _req: &mut Request| {
            let deadline_visible = ctx.deadline().is_some() && !ctx.deadline_exceeded();
            async move {
                let body = if deadline_visible {
                    "deadline-ok"
                } else {
                    "deadline-missing"
                };
                Response::with_status(StatusCode::OK)
                    .body(ResponseBody::Bytes(body.as_bytes().to_vec()))
            }
        })
        .build();
    let config = ServerConfig::new("127.0.0.1:0").with_request_timeout(Time::from_secs(30));
    let (_server, addr, _server_thread) = spawn_app_server(app, config);

    let response = get_response(addr, "/fast");
    let head = String::from_utf8_lossy(&response);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "in-budget handler must be delivered unchanged, got: {head}"
    );
    assert!(
        head.contains("deadline-ok"),
        "handler must observe an unexceeded request deadline on RequestContext, got: {head}"
    );
}
