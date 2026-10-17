//! Exercise the registered resource handlers through both protocol eras.
//! A held daemon read must yield before its response is available.

use fastmcp::{McpContext, McpError, McpResult, Outcome};
use fastmcp_server::{FinalMethodOutcome, ResourceHandler};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_mcp::resources::{DjResource, ZoneResource, ZonesResource};
use fsonos_mcp::tools::{Backend, Daemon, install};
use fsonos_proto::{ProtoError, Transport};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::net::IpAddr;
use std::sync::{Arc, mpsc};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

const WAIT: Duration = Duration::from_secs(5);
const ROOM: &str = "Room%20%2F%20%26%40S2";
const ROOM_URI: &str = "sonos://zones/Room%20%2F%20%26%40S2";
const STATE_PATH: &str = "/zones/Room%20%2F%20%26%40S2/state";
const DJ_PATH: &str = "/zones/Room%20%2F%20%26%40S2/dj";

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a selected daemon must own the resource read")
    }
}

struct Request {
    path: String,
    thread: ThreadId,
    reply: mpsc::Sender<Result<Value, Failure>>,
}

struct HeldDaemon(mpsc::Sender<Request>);

impl Daemon for HeldDaemon {
    fn call(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Option<Result<Value, Failure>> {
        assert_eq!(method, "GET");
        assert!(body.is_none());
        let (reply, answer) = mpsc::channel();
        self.0
            .send(Request {
                path: path.into(),
                thread: std::thread::current().id(),
                reply,
            })
            .unwrap();
        Some(
            answer
                .recv_timeout(WAIT)
                .expect("the request thread must be free to answer"),
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
        assert!(Instant::now() < deadline, "resource read did not finish");
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

async fn read(
    resource: &dyn ResourceHandler,
    uri: &str,
    params: &HashMap<String, String>,
    modern: bool,
) -> McpResult<Value> {
    let runtime = asupersync::runtime::RuntimeBuilder::new()
        .worker_threads(1)
        .build()
        .unwrap();
    let request_cx = runtime.request_cx_with_budget(asupersync::Budget::INFINITE);
    let context = McpContext::new(request_cx.clone(), 1);
    if modern {
        match resource
            .read_final_outcome_async_with_uri_in_request(&context, &request_cx, uri, params)
            .await
        {
            Outcome::Ok(FinalMethodOutcome::Complete(result)) => {
                Ok(serde_json::to_value(result.payload.contents).unwrap())
            }
            Outcome::Err(error) => Err(error),
            other => panic!("unexpected modern resource outcome: {other:?}"),
        }
    } else {
        match resource.read_async_with_uri(&context, uri, params).await {
            Outcome::Ok(contents) => Ok(serde_json::to_value(contents).unwrap()),
            Outcome::Err(error) => Err(error),
            other => panic!("unexpected legacy resource outcome: {other:?}"),
        }
    }
}

fn zone() -> Value {
    json!({
        "coordinator_room": "Room / &",
        "members": ["Room / &"],
        "household": "S2",
        "transport_state": "playing"
    })
}

fn room_params() -> HashMap<String, String> {
    HashMap::from([("room".into(), ROOM.into())])
}

fn assert_document(
    requests: &mpsc::Receiver<Request>,
    resource: &dyn ResourceHandler,
    uri: &str,
    modern: bool,
    script: Vec<(&str, Value)>,
    expected: &Value,
) {
    let params = if uri == ROOM_URI {
        room_params()
    } else {
        HashMap::new()
    };
    drop(read(resource, uri, &params, modern));
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
    let mut waiting = Box::pin(read(resource, uri, &params, modern));
    let wake = waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&wake))
            .is_pending()
    );
    for (path, answer) in script {
        let request = requests.recv_timeout(WAIT).unwrap();
        assert_eq!(request.path, path);
        assert_ne!(request.thread, std::thread::current().id());
        request.reply.send(Ok(answer)).unwrap();
    }
    let contents = finish(waiting).unwrap();
    assert_eq!(contents.as_array().unwrap().len(), 1);
    assert_eq!(contents[0]["uri"], uri);
    assert_eq!(contents[0]["mimeType"], "application/json");
    let document: Value = serde_json::from_str(contents[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(&document, expected);
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

fn assert_failure(requests: &mpsc::Receiver<Request>, modern: bool, failure: Failure) {
    let expected = if (400..500).contains(&failure.status()) {
        McpError::invalid_params(failure.tool_text())
    } else {
        McpError::tool_error(failure.tool_text())
    };
    let params = HashMap::new();
    let mut waiting = Box::pin(read(&ZonesResource, "sonos://zones", &params, modern));
    let wake = waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&wake))
            .is_pending()
    );
    let request = requests.recv_timeout(WAIT).unwrap();
    assert_eq!(request.path, "/zones");
    request.reply.send(Err(failure)).unwrap();
    let error = finish(waiting).unwrap_err();
    assert_eq!(error.code, expected.code);
    assert_eq!(error.message, expected.message);
    assert!(matches!(
        requests.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn resource_reads_yield_and_preserve_documents_and_errors_in_both_protocol_eras() {
    let (send, requests) = mpsc::channel();
    let backend = Backend::new(
        Box::new(NoIo),
        Box::new(|_| panic!("a remote resource must not survey the local LAN")),
        Policy::default(),
        Client::McpStdio,
        Box::new(SystemClock),
    )
    .with_daemon(Arc::new(HeldDaemon(send)));
    assert!(install(backend));
    let state = json!({"zone": zone(), "transport_state": "playing", "volume": 37});
    let moods = json!({"moods": [], "programs": [], "now": {"source": "none"}});
    let status = json!({"zone": "Room / &@S2", "running": true, "steering": {"source": "none"}});
    for modern in [false, true] {
        assert_document(
            &requests,
            &ZonesResource,
            "sonos://zones",
            modern,
            vec![("/zones", json!([zone()]))],
            &json!({"zones": [zone()]}),
        );
        assert_document(
            &requests,
            &ZoneResource,
            ROOM_URI,
            modern,
            vec![(STATE_PATH, state.clone())],
            &state,
        );
        assert_document(
            &requests,
            &DjResource,
            "sonos://dj",
            modern,
            vec![
                ("/zones", json!([zone()])),
                ("/dj/moods", moods.clone()),
                (DJ_PATH, status.clone()),
            ],
            &json!({"zones": [status.clone()], "moods": moods.clone()}),
        );
        for code in [ErrorCode::PolicyDenied, ErrorCode::NotReady] {
            assert_failure(
                &requests,
                modern,
                Failure::new(code, "daemon refused this read"),
            );
        }
        let params = HashMap::from([("room".into(), "%ff".into())]);
        let uri = "sonos://zones/%ff";
        let error = finish(read(&ZoneResource, uri, &params, modern)).unwrap_err();
        assert_eq!(error.code, McpError::invalid_params("").code);
        assert!(error.message.contains("percent-encoded UTF-8"));
        assert!(matches!(
            requests.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }
}
