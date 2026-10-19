//! Common queue and DJ handlers must yield while their daemon call is held.
//! Exercise the generated handlers in both protocol eras, not just Backend.

use fastmcp::{McpContext, McpError, Outcome};
use fastmcp_server::ToolHandler;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_mcp::tools::dj_feedback::DjFeedback;
use fsonos_mcp::tools::dj_prefs::{DjPrefer, DjPreferences};
use fsonos_mcp::tools::dj_sync::DjSyncStatus;
use fsonos_mcp::tools::{AddToQueue, Backend, Daemon, install};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a daemon failure must not replay a queue or DJ operation locally")
    }
}

struct Request {
    method: String,
    path: String,
    thread: ThreadId,
    reply: mpsc::Sender<Result<Value, Failure>>,
}

struct HeldDaemon(mpsc::Sender<Request>);

impl Daemon for HeldDaemon {
    fn call(&self, method: &str, path: &str, _: Option<&Value>) -> Option<Result<Value, Failure>> {
        let (reply, answer) = mpsc::channel();
        self.0
            .send(Request {
                method: method.into(),
                path: path.into(),
                thread: std::thread::current().id(),
                reply,
            })
            .unwrap();
        Some(
            answer
                .recv_timeout(WAIT)
                .expect("the request thread must remain free to release the daemon reply"),
        )
    }
}

struct Wake(std::thread::Thread);

impl std::task::Wake for Wake {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

fn waker() -> Waker {
    Waker::from(Arc::new(Wake(std::thread::current())))
}

fn finish<F: Future>(future: F) -> F::Output {
    let wake = waker();
    let mut context = Context::from_waker(&wake);
    let mut future = std::pin::pin!(future);
    let deadline = Instant::now() + WAIT;
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        assert!(
            Instant::now() < deadline,
            "tool did not finish after its daemon replied"
        );
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

async fn call(tool: &dyn ToolHandler, arguments: Value, modern: bool) -> McpError {
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(1)
        .build()
        .unwrap();
    let request_cx = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let context = McpContext::new(request_cx.clone(), 1);
    if modern {
        match tool
            .call_final_outcome_async_in_request(&context, &request_cx, arguments)
            .await
        {
            Outcome::Err(error) => error,
            _ => panic!("expected a terminal daemon failure in the final tool outcome"),
        }
    } else {
        match tool.call_async(&context, arguments).await {
            Outcome::Err(error) => error,
            other => panic!("unexpected legacy tool outcome: {other:?}"),
        }
    }
}

struct Case {
    handler: Box<dyn ToolHandler>,
    arguments: Value,
    method: &'static str,
    path: &'static str,
}

fn cases() -> Vec<Case> {
    vec![
        Case {
            handler: Box::new(AddToQueue),
            arguments: json!({"zone": "Kitchen@S2", "favorite": "17"}),
            method: "POST",
            path: "/queue",
        },
        Case {
            handler: Box::new(DjFeedback),
            arguments: json!({"signal": "like", "zone": "Kitchen@S2"}),
            method: "POST",
            path: "/dj/feedback",
        },
        Case {
            handler: Box::new(DjPreferences),
            arguments: json!({}),
            method: "GET",
            path: "/dj/preferences",
        },
        Case {
            handler: Box::new(DjPrefer),
            arguments: json!({"key": "energy", "value": "30"}),
            method: "POST",
            path: "/dj/preferences",
        },
        Case {
            handler: Box::new(DjSyncStatus),
            arguments: json!({}),
            method: "GET",
            path: "/dj/sync",
        },
    ]
}

#[test]
fn queue_and_dj_handlers_yield_and_preserve_terminal_daemon_failures_in_both_eras() {
    let (send, requests) = mpsc::channel();
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("a selected daemon must not trigger a local survey")),
        Policy::default(),
        Client::McpStdio,
        Box::new(SystemClock),
    )
    .with_daemon(Arc::new(HeldDaemon(send)));
    assert!(install(backend));
    for modern in [false, true] {
        for case in cases() {
            drop(call(&*case.handler, case.arguments.clone(), modern));
            assert!(matches!(
                requests.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
            let mut waiting = Box::pin(call(&*case.handler, case.arguments, modern));
            let wake = waker();
            assert!(
                waiting
                    .as_mut()
                    .poll(&mut Context::from_waker(&wake))
                    .is_pending(),
                "{} must yield while the daemon is blocked",
                case.path,
            );
            let request = requests.recv_timeout(WAIT).unwrap();
            assert_eq!(
                (request.method.as_str(), request.path.as_str()),
                (case.method, case.path)
            );
            assert_ne!(request.thread, std::thread::current().id());
            let failure = Failure::new(ErrorCode::PlayerUnreachable, "reply lost after dispatch");
            let expected = McpError::tool_error(failure.tool_text());
            request.reply.send(Err(failure)).unwrap();
            let error = finish(waiting);
            assert_eq!(error.code, expected.code);
            assert_eq!(error.message, expected.message);
            assert!(matches!(
                requests.try_recv(),
                Err(mpsc::TryRecvError::Empty)
            ));
        }
    }
}
