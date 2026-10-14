//! GitHub #65: one server process must answer more than one modern request.
//!
//! A no-default-features server (the production `run_loop_pump_with_policy`
//! branch, not the `cfg(test)` dual-era implementation) answered
//! `server/discover` on stdio and then stopped producing responses: the next
//! `tools/list` on the same connection never got a reply and the process kept
//! waiting on stdin.
//!
//! This target is an ordinary integration test, so the library it links is the
//! one a downstream consumer gets. Built with `--no-default-features` it
//! exercises the shipped no-legacy dispatcher; built with the crate's default
//! `legacy-2024-11-05` it exercises the dual-era one. Both must handle the same
//! sequence, so the assertion is meaningful under either profile and the
//! `--no-default-features` run is the one that pins the regression.
//!
//! Every scenario runs on a real asupersync runtime (`fastmcp_core::block_on`),
//! never `Cx::for_testing`, and is bounded by a join deadline on a worker
//! thread: a regression fails the test instead of hanging CI.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{McpContext, McpOutcome, McpResult, block_on};
use fastmcp_derive::tool;
use fastmcp_protocol::{Content, JsonRpcMessage, JsonRpcRequest, JsonRpcResponse, Tool};
use fastmcp_server::{FinalToolOutcome, Server, ToolHandler};
use fastmcp_transport::{Codec, Transport, TransportError, TransportRecvHalf, TransportSendHalf};

/// A regression must fail, not hang. Generous next to the milliseconds a
/// scripted in-memory transport needs (a healthy run finishes in well under a
/// second), and far below any CI job timeout, with enough slack that a loaded
/// shared runner cannot turn scheduling latency into a false red.
const SCENARIO_DEADLINE: Duration = Duration::from_secs(60);

const MODERN_PROTOCOL_VERSION: &str = "2026-07-28";

#[tool(name = "echo", description = "Returns its argument unchanged")]
fn echo(ctx: &McpContext, value: String) -> McpResult<String> {
    ctx.checkpoint()?;
    Ok(value)
}

/// The exact `_meta` envelope the reporter's wire transcript carries.
fn modern_meta() -> serde_json::Value {
    serde_json::json!({
        "io.modelcontextprotocol/protocolVersion": MODERN_PROTOCOL_VERSION,
        "io.modelcontextprotocol/clientCapabilities": {"tools": {"listChanged": true}},
        "io.modelcontextprotocol/clientInfo": {
            "name": "stdio-multi-request-repro",
            "version": "0.0.1",
        },
    })
}

fn modern_request(method: &str, id: i64, extra: Option<serde_json::Value>) -> JsonRpcRequest {
    let mut params = serde_json::json!({ "_meta": modern_meta() });
    if let Some(serde_json::Value::Object(fields)) = extra {
        let object = params
            .as_object_mut()
            .expect("modern request params are a JSON object");
        for (key, value) in fields {
            object.insert(key, value);
        }
    }
    JsonRpcRequest::new(method, Some(params), id)
}

#[derive(Default)]
struct ScriptedState {
    incoming: VecDeque<JsonRpcMessage>,
    outgoing: Vec<JsonRpcMessage>,
    closed: bool,
}

/// An in-memory full-duplex stand-in for the stdio pipe: it replays a fixed
/// request script and then reports `Closed`, exactly as a client that writes
/// its requests and closes stdin does.
struct ScriptedTransport {
    state: Arc<Mutex<ScriptedState>>,
}

#[derive(Clone)]
struct ScriptedProbe(Arc<Mutex<ScriptedState>>);

impl ScriptedTransport {
    fn new(requests: Vec<JsonRpcRequest>) -> (Self, ScriptedProbe) {
        let state = Arc::new(Mutex::new(ScriptedState {
            incoming: requests.into_iter().map(JsonRpcMessage::Request).collect(),
            ..ScriptedState::default()
        }));
        (
            Self {
                state: Arc::clone(&state),
            },
            ScriptedProbe(state),
        )
    }
}

impl ScriptedProbe {
    fn responses(&self) -> Vec<JsonRpcResponse> {
        self.0
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .outgoing
            .iter()
            .filter_map(|message| match message {
                JsonRpcMessage::Response(response) => Some(response.clone()),
                _ => None,
            })
            .collect()
    }

    fn closed(&self) -> bool {
        self.0
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .closed
    }
}

impl Transport for ScriptedTransport {
    fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .outgoing
            .push(message.clone());
        Ok(())
    }

    fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .incoming
            .pop_front()
            .ok_or(TransportError::Closed)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .closed = true;
        Ok(())
    }
}

struct ScenarioOutcome {
    run: McpResult<()>,
    responses: Vec<JsonRpcResponse>,
    closed: bool,
}

/// Run one request script through the public transport runtime on a real
/// asupersync runtime, bounded by [`SCENARIO_DEADLINE`].
///
/// The pump is driven on its own thread and the outcome is handed back over a
/// channel, so a dispatcher that stops answering fails the assertion below
/// instead of parking the test process forever.
fn run_scenario(label: &'static str, requests: Vec<JsonRpcRequest>) -> ScenarioOutcome {
    let (transport, probe) = ScriptedTransport::new(requests);
    let (tx, rx) = mpsc::channel();
    let probe_for_thread = probe.clone();
    let worker = std::thread::Builder::new()
        .name(format!("srv-65-{label}"))
        .spawn(move || {
            let run = block_on(async move {
                let cx = Cx::current().expect("the asupersync runtime installs a current Cx");
                let server = Server::new("srv-65-multi-request", "1.0.0")
                    .tool(Echo)
                    .build();
                // Transport::recv and this returning entry point are
                // synchronous. Keep the caller's current-thread executor
                // free to drive request children while its blocking pool
                // owns the transport loop.
                let mut pump = cx
                    .spawn_blocking(move |pump_cx| {
                        server.run_transport_returning_with_cx(&pump_cx, transport)
                    })
                    .expect("the caller runtime must admit the transport pump");
                pump.join(&cx)
                    .await
                    .expect("the caller-owned pump must report a final status")
            });
            // Send before the thread ends so the receiver never waits on a
            // join that a panicking dispatcher would never complete.
            let _ = tx.send(ScenarioOutcome {
                run,
                responses: probe_for_thread.responses(),
                closed: probe_for_thread.closed(),
            });
        })
        .expect("the scenario worker thread must start");

    match rx.recv_timeout(SCENARIO_DEADLINE) {
        Ok(outcome) => {
            worker.join().expect("the scenario worker must not panic");
            outcome
        }
        Err(timeout) => panic!(
            "[{label}] the server stopped answering ({timeout}): no outcome within \
             {SCENARIO_DEADLINE:?}. Responses observed so far: {:?}",
            probe
                .responses()
                .iter()
                .map(|response| response.id.clone())
                .collect::<Vec<_>>()
        ),
    }
}

/// The receive half of [`ScriptedTransport`], for the split entry point.
struct ScriptedRecvHalf {
    state: Arc<Mutex<ScriptedState>>,
}

/// The send half of [`ScriptedTransport`], for the split entry point.
struct ScriptedSendHalf {
    state: Arc<Mutex<ScriptedState>>,
}

impl TransportRecvHalf for ScriptedRecvHalf {
    fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .incoming
            .pop_front()
            .ok_or(TransportError::Closed)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .closed = true;
        Ok(())
    }
}

impl TransportSendHalf for ScriptedSendHalf {
    fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        self.state
            .lock()
            .expect("scripted transport mutex must not be poisoned")
            .outgoing
            .push(message.clone());
        Ok(())
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        Ok(())
    }
}

fn scripted_halves(
    requests: Vec<JsonRpcRequest>,
) -> (ScriptedRecvHalf, ScriptedSendHalf, ScriptedProbe) {
    let state = Arc::new(Mutex::new(ScriptedState {
        incoming: requests.into_iter().map(JsonRpcMessage::Request).collect(),
        ..ScriptedState::default()
    }));
    (
        ScriptedRecvHalf {
            state: Arc::clone(&state),
        },
        ScriptedSendHalf {
            state: Arc::clone(&state),
        },
        ScriptedProbe(state),
    )
}

/// Run a request script in the exact arrangement `Server::run_stdio_with_cx`
/// uses: the receive pump is a caller-owned BLOCKING child, and the caller's
/// runtime context is handed in separately as the dispatch context.
///
/// This is the shape the report was filed against — the production no-legacy
/// pump names that parameter `_dispatch_cx` and drives request futures from the
/// blocking receive-pump context instead — so the multi-request contract has to
/// hold here, not only on the simpler same-context transport entry point.
fn run_stdio_shaped_scenario(
    label: &'static str,
    requests: Vec<JsonRpcRequest>,
) -> ScenarioOutcome {
    let (recv_half, send_half, probe) = scripted_halves(requests);
    let (tx, rx) = mpsc::channel();
    let probe_for_thread = probe.clone();
    let worker = std::thread::Builder::new()
        .name(format!("srv-65-stdio-{label}"))
        .spawn(move || {
            let run = block_on(async move {
                let cx = Cx::current().expect("the asupersync runtime installs a current Cx");
                let dispatch_cx = cx.clone();
                let server = Server::new("srv-65-stdio-shaped", "1.0.0")
                    .tool(Echo)
                    .build();
                let mut pump = match cx.spawn_blocking(move |pump_cx| {
                    server.run_split_transport_returning_with_dispatch_cx(
                        &pump_cx,
                        &dispatch_cx,
                        recv_half,
                        send_half,
                    )
                }) {
                    Ok(pump) => pump,
                    Err(error) => {
                        panic!(
                            "the caller runtime must admit the pump as a blocking child: {error:?}"
                        )
                    }
                };
                pump.join(&cx)
                    .await
                    .expect("the caller-owned pump must report a final status")
            });
            let _ = tx.send(ScenarioOutcome {
                run,
                responses: probe_for_thread.responses(),
                closed: probe_for_thread.closed(),
            });
        })
        .expect("the scenario worker thread must start");

    match rx.recv_timeout(SCENARIO_DEADLINE) {
        Ok(outcome) => {
            worker.join().expect("the scenario worker must not panic");
            outcome
        }
        Err(timeout) => panic!(
            "[{label}] the stdio-shaped pump stopped answering ({timeout}): no outcome \
             within {SCENARIO_DEADLINE:?}. Responses observed so far: {:?}",
            probe
                .responses()
                .iter()
                .map(|response| response.id.clone())
                .collect::<Vec<_>>()
        ),
    }
}

/// Find the response correlated to `expected_id`.
///
/// JSON-RPC responses are correlated by id, not by arrival order, and the
/// dual-era dispatcher answers concurrently — so the contract under test is
/// "every request is answered", never "answers arrive in request order".
fn response_for<'a>(
    responses: &'a [JsonRpcResponse],
    expected_id: i64,
    label: &str,
) -> &'a JsonRpcResponse {
    let wanted = JsonRpcRequest::new("probe", None, expected_id).id;
    responses
        .iter()
        .find(|response| response.id == wanted)
        .unwrap_or_else(|| {
            panic!(
                "{label}: no response correlated to request id {expected_id}; got {:?}",
                responses
                    .iter()
                    .map(|response| response.id.clone())
                    .collect::<Vec<_>>()
            )
        })
}

fn assert_ok_response(response: &JsonRpcResponse, label: &str) {
    assert!(
        response.error.is_none(),
        "{label}: expected a result, got error {:?}",
        response.error
    );
}

/// Acceptance criterion 3: one process answers
/// `server/discover -> tools/list -> tools/call -> tools/list`.
#[test]
fn srv_65_one_process_answers_a_modern_request_sequence() {
    let outcome = run_scenario(
        "sequence",
        vec![
            modern_request("server/discover", 1, None),
            modern_request("tools/list", 2, None),
            modern_request(
                "tools/call",
                3,
                Some(serde_json::json!({
                    "name": "echo",
                    "arguments": {"value": "pong"},
                })),
            ),
            modern_request("tools/list", 4, None),
        ],
    );

    assert!(
        outcome.run.is_ok(),
        "the modern transport runtime must close cleanly: {:?}",
        outcome.run.as_ref().err()
    );
    assert_eq!(
        outcome.responses.len(),
        4,
        "every modern request on one connection must be answered; got ids {:?}",
        outcome
            .responses
            .iter()
            .map(|response| response.id.clone())
            .collect::<Vec<_>>()
    );
    for expected_id in [1_i64, 2, 3, 4] {
        assert_ok_response(
            response_for(&outcome.responses, expected_id, "modern request sequence"),
            "modern request sequence",
        );
    }
    let discover = response_for(&outcome.responses, 1, "modern request sequence")
        .result
        .as_ref()
        .expect("server/discover returns a result");
    assert_eq!(
        discover.get("supportedVersions"),
        Some(&serde_json::json!([MODERN_PROTOCOL_VERSION])),
        "the first response must still be the modern discovery result"
    );
    let tools = response_for(&outcome.responses, 2, "modern request sequence")
        .result
        .as_ref()
        .and_then(|result| result.get("tools"))
        .and_then(serde_json::Value::as_array)
        .expect("tools/list returns a tools array");
    assert!(
        tools
            .iter()
            .any(|tool| tool.get("name") == Some(&serde_json::json!("echo"))),
        "the second response must be this server's real tool catalog: {tools:?}"
    );
    assert!(
        outcome.closed,
        "the runtime must close the transport it owned"
    );
}

/// The minimal transcript from the report: discover then tools/list.
#[test]
fn srv_65_second_request_after_discover_is_answered() {
    let outcome = run_scenario(
        "discover-then-list",
        vec![
            modern_request("server/discover", 1, None),
            modern_request("tools/list", 2, None),
        ],
    );

    assert!(
        outcome.run.is_ok(),
        "the two-request transcript must close cleanly: {:?}",
        outcome.run.as_ref().err()
    );
    assert_eq!(
        outcome.responses.len(),
        2,
        "the reported transcript produced only the server/discover response"
    );
    assert_ok_response(
        response_for(&outcome.responses, 1, "discover then list"),
        "server/discover",
    );
    assert_ok_response(
        response_for(&outcome.responses, 2, "discover then list"),
        "tools/list after discover",
    );
}

/// Planted negative: the era gate must still refuse a second request that
/// drops the modern protocol version, so the fix above cannot be a blanket
/// "answer everything" relaxation.
#[test]
fn srv_65_second_request_without_modern_metadata_is_refused() {
    let outcome = run_scenario(
        "downgrade",
        vec![
            modern_request("server/discover", 1, None),
            JsonRpcRequest::new("tools/list", Some(serde_json::json!({})), 2_i64),
        ],
    );

    assert_eq!(
        outcome.responses.len(),
        2,
        "the refusal itself must be delivered, not silently dropped"
    );
    assert_ok_response(
        response_for(&outcome.responses, 1, "downgrade"),
        "server/discover",
    );
    assert!(
        response_for(&outcome.responses, 2, "downgrade")
            .error
            .is_some(),
        "a second request without the 2026-07-28 envelope must be refused, not served"
    );
}

/// Acceptance criterion 4, in the arrangement the report used: the blocking
/// receive pump plus a separate caller-owned dispatch context must still admit
/// every request on the connection, not just the first.
#[test]
fn srv_65_stdio_shaped_pump_answers_a_modern_request_sequence() {
    let outcome = run_stdio_shaped_scenario(
        "sequence",
        vec![
            modern_request("server/discover", 1, None),
            modern_request("tools/list", 2, None),
            modern_request(
                "tools/call",
                3,
                Some(serde_json::json!({
                    "name": "echo",
                    "arguments": {"value": "pong"},
                })),
            ),
            modern_request("tools/list", 4, None),
        ],
    );

    assert!(
        outcome.run.is_ok(),
        "the stdio-shaped pump must close cleanly: {:?}",
        outcome.run.as_ref().err()
    );
    assert_eq!(
        outcome.responses.len(),
        4,
        "the stdio-shaped pump answered only {} of 4 modern requests (ids {:?})",
        outcome.responses.len(),
        outcome
            .responses
            .iter()
            .map(|response| response.id.clone())
            .collect::<Vec<_>>()
    );
    for expected_id in [1_i64, 2, 3, 4] {
        assert_ok_response(
            response_for(
                &outcome.responses,
                expected_id,
                "stdio-shaped request sequence",
            ),
            "stdio-shaped request sequence",
        );
    }
    assert!(
        outcome.closed,
        "the stdio-shaped runtime must close the receive half it owned"
    );
}

#[derive(Default)]
struct PendingControl {
    entered: AtomicUsize,
    active: AtomicUsize,
    released: AtomicBool,
}

struct PendingTool(Arc<PendingControl>);

struct ActiveInvocation(Arc<PendingControl>);

impl Drop for ActiveInvocation {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ToolHandler for PendingTool {
    fn execution_mode(&self) -> fastmcp_server::ToolExecutionMode {
        fastmcp_server::ToolExecutionMode::Async
    }

    fn definition(&self) -> Tool {
        let mut definition = Echo.definition();
        definition.name = "pending".to_owned();
        definition
    }

    fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
        Echo.call(ctx, arguments)
    }

    fn call_final_outcome_async<'a>(
        &'a self,
        ctx: &'a McpContext,
        arguments: serde_json::Value,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = McpOutcome<FinalToolOutcome>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.0.active.fetch_add(1, Ordering::AcqRel);
            let _active = ActiveInvocation(Arc::clone(&self.0));
            self.0.entered.fetch_add(1, Ordering::AcqRel);
            while !self.0.released.load(Ordering::Acquire) {
                if let Err(error) = ctx.checkpoint() {
                    return McpOutcome::Err(error.into());
                }
                asupersync::time::sleep(ctx.cx().now(), Duration::from_millis(5)).await;
            }
            match Echo.call_final(ctx, arguments) {
                Ok(result) => McpOutcome::Ok(FinalToolOutcome::Complete(result)),
                Err(error) => McpOutcome::Err(error),
            }
        })
    }
}

struct WireRecv(BufReader<TcpStream>);

#[derive(Default)]
struct WireSendControl {
    fail_writes: AtomicBool,
    cancel_after_response: AtomicBool,
    cancelled: AtomicUsize,
}

struct WireSend(TcpStream, Arc<AtomicUsize>, Arc<WireSendControl>);

fn read_wire(reader: &mut BufReader<TcpStream>) -> Result<JsonRpcMessage, TransportError> {
    let mut line = String::new();
    if reader.read_line(&mut line)? == 0 {
        return Err(TransportError::Closed);
    }
    Codec::new()
        .decode_complete_message(line.as_bytes())
        .map_err(TransportError::Codec)
}

fn write_wire(stream: &mut TcpStream, message: &JsonRpcMessage) -> Result<(), TransportError> {
    let codec = Codec::new();
    let bytes = match message {
        JsonRpcMessage::Request(request) => codec.encode_request(request),
        JsonRpcMessage::Response(response) => codec.encode_response(response),
    }
    .map_err(TransportError::Codec)?;
    stream.write_all(&bytes)?;
    Ok(())
}

impl TransportRecvHalf for WireRecv {
    fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        read_wire(&mut self.0)
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.0.get_ref().shutdown(Shutdown::Read)?;
        Ok(())
    }
}

impl TransportSendHalf for WireSend {
    fn send(&mut self, cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        let result = if self.2.fail_writes.load(Ordering::Acquire) {
            Err(TransportError::Io(std::io::Error::from(
                std::io::ErrorKind::BrokenPipe,
            )))
        } else {
            eprintln!(
                "modern-pump outbound={}",
                serde_json::to_string(message).unwrap()
            );
            write_wire(&mut self.0, message)
        };
        if self.2.cancel_after_response.swap(false, Ordering::AcqRel) {
            // Cancel the actual dispatch child after the handler's final
            // checkpoint. No later checkpoint acknowledges this cancellation,
            // so the runtime must publish JoinError::Cancelled for that child.
            cx.cancel_with(
                asupersync::CancelKind::User,
                Some("deliberate cancellation at response completion"),
            );
            self.2.cancelled.fetch_add(1, Ordering::Release);
        }
        if result.is_ok() {
            self.1.fetch_add(1, Ordering::Release);
        }
        result
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        self.0.shutdown(Shutdown::Write)?;
        Ok(())
    }
}

/// Real socket bytes and the public split runner. A single async worker owns
/// the request children while the receive pump occupies a blocking worker.
/// Cleanup releases even a regressed sequential handler before joining it.
struct WireScenario {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    control: Arc<PendingControl>,
    #[cfg(not(feature = "legacy-2024-11-05"))]
    send_control: Arc<WireSendControl>,
    shutdown: Arc<AtomicBool>,
    shutdown_active: Arc<AtomicUsize>,
    outcome: mpsc::Receiver<McpResult<()>>,
    worker: Option<std::thread::JoinHandle<()>>,
    subject: String,
    input_closed: bool,
}

impl WireScenario {
    fn start() -> Self {
        Self::start_with_dispatch(true)
    }

    fn start_with_dispatch(separate_dispatch: bool) -> Self {
        Self::start_with_options(separate_dispatch, false)
    }

    fn start_with_options(separate_dispatch: bool, hold_first_poll: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_socket, peer) = listener.accept().unwrap();
        writer
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        writer
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        server_socket
            .set_read_timeout(Some(SCENARIO_DEADLINE))
            .unwrap();
        server_socket
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let reader = BufReader::new(writer.try_clone().unwrap());
        let control = Arc::new(PendingControl::default());
        let send_control = Arc::new(WireSendControl::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_active = Arc::new(AtomicUsize::new(usize::MAX));
        let shutdown_probe = Arc::clone(&shutdown);
        let shutdown_active_probe = Arc::clone(&shutdown_active);
        let shutdown_control = Arc::clone(&control);
        let server = Server::new("modern-owned-pump", "1.0.0")
            .tool(Echo)
            .tool(PendingTool(Arc::clone(&control)))
            .on_shutdown(move || {
                shutdown_active_probe.store(
                    shutdown_control.active.load(Ordering::Acquire),
                    Ordering::Release,
                );
                shutdown_probe.store(true, Ordering::Release);
            })
            .build();
        let response_count = Arc::new(AtomicUsize::new(0));
        let recv = WireRecv(BufReader::new(server_socket.try_clone().unwrap()));
        let send = WireSend(
            server_socket,
            Arc::clone(&response_count),
            Arc::clone(&send_control),
        );
        let (tx, outcome) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .blocking_threads(0, 4)
                .build()
                .unwrap();
            let result = runtime.block_on(async move {
                let root = Cx::current().unwrap();
                if !separate_dispatch {
                    let mut pump = root
                        .spawn_blocking(move |pump_cx| {
                            server.run_split_transport_returning_with_cx(&pump_cx, recv, send)
                        })
                        .expect("the caller runtime must admit the same-context split pump");
                    return pump.join(&root).await.expect("same-context pump must join");
                }
                let dispatch_cx = root.clone();
                let (started_tx, started_rx) = mpsc::sync_channel(1);
                let (ingress_tx, ingress_rx) = mpsc::sync_channel(1);
                let mut pump = root
                    .spawn_blocking(move |pump_cx| {
                        if hold_first_poll {
                            started_tx.send(()).unwrap();
                            ingress_rx
                                .recv_timeout(Duration::from_secs(3))
                                .expect("scheduler worker must permit ingress");
                        }
                        server.run_split_transport_returning_with_dispatch_cx(
                            &pump_cx,
                            &dispatch_cx,
                            recv,
                            send,
                        )
                    })
                    .unwrap();
                if hold_first_poll {
                    // The blocking-pool wrapper needs this current-thread
                    // executor to poll it before the pump can announce start.
                    // Keep that startup wait cooperative, then deliberately
                    // occupy the worker to test cancellation before first poll.
                    let started_deadline = std::time::Instant::now() + Duration::from_secs(3);
                    loop {
                        match started_rx.try_recv() {
                            Ok(()) => break,
                            Err(mpsc::TryRecvError::Disconnected) => {
                                panic!("blocking pump ended before announcing start")
                            }
                            Err(mpsc::TryRecvError::Empty) => {}
                        }
                        assert!(
                            std::time::Instant::now() < started_deadline,
                            "blocking pump must start before holding the worker"
                        );
                        asupersync::time::sleep(root.now(), Duration::from_millis(1)).await;
                    }
                    let mut held_worker = root
                        .spawn(move |_cx| async move {
                            // No input reaches dispatch until this task owns
                            // the only worker. The auth probe is answered by
                            // the blocking pump without polling a request child.
                            ingress_tx.send(()).unwrap();
                            let deadline = std::time::Instant::now() + Duration::from_secs(3);
                            while response_count.load(Ordering::Acquire) == 0
                                && std::time::Instant::now() < deadline
                            {
                                std::thread::sleep(Duration::from_millis(1));
                            }
                            assert_eq!(response_count.load(Ordering::Acquire), 1);
                        })
                        .expect("hold task must run on the scheduler worker");
                    held_worker
                        .join(&root)
                        .await
                        .expect("held worker must join");
                }
                pump.join(&root).await.expect("pump child must join")
            });
            let _ = tx.send(result);
        });
        Self {
            reader,
            writer,
            control,
            #[cfg(not(feature = "legacy-2024-11-05"))]
            send_control,
            shutdown,
            shutdown_active,
            outcome,
            worker: Some(worker),
            subject: format!("socket-{peer}"),
            input_closed: false,
        }
    }

    fn send(&mut self, request: JsonRpcRequest) {
        write_wire(&mut self.writer, &JsonRpcMessage::Request(request)).unwrap();
    }

    fn response(&mut self, id: i64) -> JsonRpcResponse {
        let message = read_wire(&mut self.reader)
            .expect("request must receive a response within the wire deadline");
        let JsonRpcMessage::Response(response) = message else {
            panic!("expected response, got {message:?}");
        };
        assert_eq!(response.id, Some(id.into()), "wrong correlated response");
        response
    }

    fn pending(&mut self, id: i64) {
        self.send(modern_request(
            "tools/call",
            id,
            Some(serde_json::json!({
                "name": "pending", "arguments": {"value": self.subject},
            })),
        ));
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while self.control.entered.load(Ordering::Acquire) == 0
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(
            self.control.entered.load(Ordering::Acquire),
            1,
            "pending handler must actually start; shutdown={}",
            self.shutdown.load(Ordering::Acquire)
        );
    }

    fn cancel(&mut self, id: i64) {
        let mut request = modern_request(
            "notifications/cancelled",
            0,
            Some(serde_json::json!({"requestId": id})),
        );
        request.id = None;
        self.send(request);
    }

    #[cfg(not(feature = "legacy-2024-11-05"))]
    fn cancel_response_child(&self, fail_write: bool) {
        self.send_control
            .fail_writes
            .store(fail_write, Ordering::Release);
        self.send_control
            .cancel_after_response
            .store(true, Ordering::Release);
    }

    fn close_input(&mut self) {
        assert!(!self.input_closed, "input EOF is emitted exactly once");
        self.writer.shutdown(Shutdown::Write).unwrap();
        self.input_closed = true;
    }

    fn finish(&mut self) {
        self.control.released.store(true, Ordering::Release);
        if !self.input_closed {
            self.close_input();
        }
        let result = self
            .outcome
            .recv_timeout(SCENARIO_DEADLINE)
            .expect("pump must settle before cleanup");
        self.worker.take().unwrap().join().unwrap();
        assert!(result.is_ok(), "returning pump failed: {result:?}");
        assert!(self.shutdown.load(Ordering::Acquire));
        assert_eq!(
            self.shutdown_active.load(Ordering::Acquire),
            0,
            "shutdown must follow handler cleanup"
        );
    }
}

impl Drop for WireScenario {
    fn drop(&mut self) {
        self.control.released.store(true, Ordering::Release);
        let _ = self.writer.shutdown(Shutdown::Write);
        if self.worker.is_some() && self.outcome.recv_timeout(SCENARIO_DEADLINE).is_ok() {
            let _ = self.worker.take().unwrap().join();
        }
    }
}

#[test]
fn srv_65_modern_pending_request_allows_other_requests() {
    // The no-legacy synchronous split entry point must also make progress
    // when its caller supplies the same context for ingress and dispatch.
    // The unchanged dual-era pump requires a separate dispatch Cx.
    let dispatch_modes: &[bool] = if cfg!(feature = "legacy-2024-11-05") {
        &[true]
    } else {
        &[true, false]
    };
    for &separate_dispatch in dispatch_modes {
        let mut wire = WireScenario::start_with_dispatch(separate_dispatch);
        wire.pending(10);
        wire.send(modern_request("tools/list", 11, None));
        assert_ok_response(&wire.response(11), "catalog while handler pending");
        assert_eq!(wire.control.active.load(Ordering::Acquire), 1);
        wire.control.released.store(true, Ordering::Release);
        let response = wire.response(10);
        assert_ok_response(&response, "released handler");
        assert_eq!(response.result.unwrap()["content"][0]["text"], wire.subject);
        wire.finish();
    }
}

#[test]
fn srv_65_modern_cancellation_preserves_unrelated_request() {
    if !cfg!(feature = "legacy-2024-11-05") {
        for cancellation_id in [50, 999] {
            let mut queued = WireScenario::start_with_options(true, true);
            queued.control.released.store(true, Ordering::Release);
            queued.send(modern_request(
                "tools/call",
                50,
                Some(serde_json::json!({
                    "name": "pending", "arguments": {"value": queued.subject},
                })),
            ));
            queued.cancel(cancellation_id);
            // Admission rejects credentials without an auth provider on the
            // pump itself, before submitting an async child. Its response
            // proves the preceding cancellation was processed while the
            // original request still could not receive its first poll.
            queued.send(modern_request(
                "tools/list",
                52,
                Some(serde_json::json!({"token": "unadmitted-peer"})),
            ));
            assert_eq!(
                queued.response(52).error.unwrap().code,
                fastmcp_core::McpErrorCode::ResourceForbidden.into()
            );
            queued.send(modern_request("tools/list", 51, None));
            let responses = (0..2)
                .map(|_| match read_wire(&mut queued.reader).unwrap() {
                    JsonRpcMessage::Response(response) => response,
                    message => panic!("expected correlated response, got {message:?}"),
                })
                .collect::<Vec<_>>();
            let original = response_for(&responses, 50, "pre-poll cancellation");
            if cancellation_id == 50 {
                assert_eq!(
                    original.error.as_ref().unwrap().code,
                    fastmcp_core::McpErrorCode::RequestCancelled.into()
                );
                assert_eq!(queued.control.entered.load(Ordering::Acquire), 0);
            } else {
                assert_ok_response(original, "unknown target preserves queued request");
                assert_eq!(
                    original.result.as_ref().unwrap()["content"][0]["text"],
                    queued.subject
                );
                assert_eq!(queued.control.entered.load(Ordering::Acquire), 1);
            }
            assert_ok_response(
                response_for(&responses, 51, "pre-poll cancellation"),
                "catalog survives pre-poll cancellation",
            );
            queued.finish();
        }
    }
    let mut wire = WireScenario::start();
    wire.pending(20);
    let mut unauthenticated = modern_request(
        "notifications/cancelled",
        0,
        Some(serde_json::json!({"requestId": 20, "token": "unadmitted-peer"})),
    );
    unauthenticated.id = None;
    wire.send(unauthenticated);
    wire.send(modern_request("tools/list", 23, None));
    assert_ok_response(&wire.response(23), "catalog after rejected credentials");
    assert_eq!(
        wire.control.active.load(Ordering::Acquire),
        1,
        "unauthenticated cancellation must leave the handler running"
    );
    wire.cancel(999);
    wire.send(modern_request("tools/list", 21, None));
    assert_ok_response(&wire.response(21), "catalog after unrelated cancellation");
    assert_eq!(
        wire.control.active.load(Ordering::Acquire),
        1,
        "unknown ID must not cancel the active request"
    );
    wire.cancel(20);
    let response = wire.response(20);
    assert_eq!(
        response.error.unwrap().code,
        fastmcp_core::McpErrorCode::RequestCancelled.into()
    );
    assert_eq!(wire.control.active.load(Ordering::Acquire), 0);
    wire.send(modern_request("tools/list", 22, None));
    assert_ok_response(&wire.response(22), "catalog after target cancellation");
    wire.finish();
}

#[test]
fn srv_65_modern_duplicate_id_does_not_enter_handler() {
    let mut wire = WireScenario::start();
    wire.pending(30);
    wire.send(modern_request(
        "tools/call",
        30,
        Some(serde_json::json!({
            "name": "pending", "arguments": {"value": wire.subject},
        })),
    ));
    let duplicate = wire.response(30);
    assert_eq!(
        duplicate.error.unwrap().code,
        fastmcp_core::McpErrorCode::InvalidRequest.into()
    );
    assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);
    wire.control.released.store(true, Ordering::Release);
    assert_ok_response(
        &wire.response(30),
        "original request survives duplicate refusal",
    );
    wire.finish();
}

#[test]
fn srv_65_modern_subscription_drains_before_shutdown() {
    let mut wire = WireScenario::start();
    wire.send(modern_request(
        "subscriptions/listen",
        40,
        Some(serde_json::json!({
            "notifications": {"toolsListChanged": true},
        })),
    ));
    let acknowledgement = read_wire(&mut wire.reader).unwrap();
    assert!(
        matches!(acknowledgement, JsonRpcMessage::Request(request) if request.method == "notifications/subscriptions/acknowledged")
    );
    wire.send(modern_request("tools/list", 41, None));
    assert_ok_response(&wire.response(41), "catalog while subscription pending");
    wire.close_input();
    let cancellation = read_wire(&mut wire.reader).unwrap();
    assert!(
        matches!(cancellation, JsonRpcMessage::Request(request) if request.method == "notifications/cancelled" && request.params.as_ref().unwrap()["requestId"] == 40)
    );
    let completion = wire.response(40);
    assert_ok_response(&completion, "graceful subscription completion");
    let result = completion.result.unwrap();
    assert_eq!(result["resultType"], "complete");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/subscriptionId"],
        40
    );
    wire.finish();
}

/// How long the interactive client below waits for a listen's acknowledgement
/// before it sends its next frame anyway. Longer than the server's own bound
/// for holding off `recv`, so a server that never writes the acknowledgement
/// before reading again is observed as such rather than raced.
#[cfg(not(feature = "legacy-2024-11-05"))]
const LISTEN_ACKNOWLEDGEMENT_WAIT: Duration = Duration::from_secs(10);

/// bd-4crkf: an UNSPLIT transport driven by an interactive client. After a
/// `subscriptions/listen` it sends nothing until it has seen the
/// acknowledgement (bounded), and records whether it did. The unsplit entry
/// point cannot write while `recv` blocks, so this is the shape that exposes
/// a server that reads again before acknowledging.
#[cfg(not(feature = "legacy-2024-11-05"))]
struct AcknowledgingClient {
    script: VecDeque<JsonRpcRequest>,
    outgoing: Arc<Mutex<Vec<JsonRpcMessage>>>,
    acknowledged_in_time: Arc<Mutex<Option<bool>>>,
}

#[cfg(not(feature = "legacy-2024-11-05"))]
fn is_listen_acknowledgement(message: &JsonRpcMessage) -> bool {
    matches!(message, JsonRpcMessage::Request(request)
        if request.method == "notifications/subscriptions/acknowledged")
}

#[cfg(not(feature = "legacy-2024-11-05"))]
impl Transport for AcknowledgingClient {
    fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
        self.outgoing
            .lock()
            .expect("client outgoing log must not be poisoned")
            .push(message.clone());
        Ok(())
    }

    fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
        let Some(next) = self.script.pop_front() else {
            return Err(TransportError::Closed);
        };
        if next.method == "notifications/cancelled" {
            let deadline = std::time::Instant::now() + LISTEN_ACKNOWLEDGEMENT_WAIT;
            let acknowledged = loop {
                if self
                    .outgoing
                    .lock()
                    .expect("client outgoing log must not be poisoned")
                    .iter()
                    .any(is_listen_acknowledgement)
                {
                    break true;
                }
                if std::time::Instant::now() >= deadline {
                    break false;
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            *self
                .acknowledged_in_time
                .lock()
                .expect("client acknowledgement record must not be poisoned") = Some(acknowledged);
        }
        Ok(JsonRpcMessage::Request(next))
    }

    fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
        Ok(())
    }
}

#[cfg(not(feature = "legacy-2024-11-05"))]
struct UnsplitListenOutcome {
    run: McpResult<()>,
    outgoing: Vec<JsonRpcMessage>,
    acknowledged_in_time: Option<bool>,
}

/// `discover -> listen(40) -> cancel(cancel_id) -> tools/list(41) -> EOF`
/// over the unsplit `run_transport_returning_with_cx`, bounded by
/// [`SCENARIO_DEADLINE`]. Before bd-4crkf the no-legacy pump ran the listen
/// inline and never read the cancellation, so this deadline is what fails.
#[cfg(not(feature = "legacy-2024-11-05"))]
fn run_unsplit_listen_scenario(label: &'static str, cancel_id: i64) -> UnsplitListenOutcome {
    let mut cancel = modern_request(
        "notifications/cancelled",
        0,
        Some(serde_json::json!({"requestId": cancel_id})),
    );
    cancel.id = None;
    let outgoing = Arc::new(Mutex::new(Vec::new()));
    let acknowledged_in_time = Arc::new(Mutex::new(None));
    let transport = AcknowledgingClient {
        script: VecDeque::from([
            modern_request("server/discover", 1, None),
            modern_request(
                "subscriptions/listen",
                40,
                Some(serde_json::json!({"notifications": {"toolsListChanged": true}})),
            ),
            cancel,
            modern_request("tools/list", 41, None),
        ]),
        outgoing: Arc::clone(&outgoing),
        acknowledged_in_time: Arc::clone(&acknowledged_in_time),
    };
    let (tx, rx) = mpsc::channel();
    let worker = std::thread::Builder::new()
        .name(format!("srv-65-{label}"))
        .spawn(move || {
            let run = block_on(async move {
                let cx = Cx::current().expect("the asupersync runtime installs a current Cx");
                let server = Server::new("srv-65-unsplit-listen", "1.0.0")
                    .tool(Echo)
                    .build();
                let mut pump = cx
                    .spawn_blocking(move |pump_cx| {
                        server.run_transport_returning_with_cx(&pump_cx, transport)
                    })
                    .expect("the caller runtime must admit the transport pump");
                pump.join(&cx)
                    .await
                    .expect("the caller-owned pump must report a final status")
            });
            let _ = tx.send(run);
        })
        .expect("the scenario worker thread must start");

    match rx.recv_timeout(SCENARIO_DEADLINE) {
        Ok(run) => {
            worker.join().expect("the scenario worker must not panic");
            UnsplitListenOutcome {
                run,
                outgoing: outgoing
                    .lock()
                    .expect("client outgoing log must not be poisoned")
                    .clone(),
                acknowledged_in_time: *acknowledged_in_time
                    .lock()
                    .expect("client acknowledgement record must not be poisoned"),
            }
        }
        Err(timeout) => panic!(
            "[{label}] the pump stopped reading while a subscriptions/listen was open \
             ({timeout}): the cancellation after it was never received. Written so far: {:?}",
            outgoing
                .lock()
                .expect("client outgoing log must not be poisoned")
        ),
    }
}

/// What both unsplit listen scenarios must show: the pump acknowledged the
/// listen before reading again, then kept reading and answered the request
/// after the cancellation, and the connection ended cleanly at EOF.
#[cfg(not(feature = "legacy-2024-11-05"))]
fn assert_unsplit_listen_kept_reading(
    outcome: &UnsplitListenOutcome,
    label: &str,
) -> Vec<JsonRpcResponse> {
    assert!(outcome.run.is_ok(), "{label}: {:?}", outcome.run);
    assert_eq!(
        outcome.acknowledged_in_time,
        Some(true),
        "{label}: the listen was not acknowledged before the pump read the next frame"
    );
    let acknowledgements: Vec<usize> = outcome
        .outgoing
        .iter()
        .enumerate()
        .filter_map(|(index, message)| is_listen_acknowledgement(message).then_some(index))
        .collect();
    assert_eq!(acknowledgements.len(), 1, "{label}: {:?}", outcome.outgoing);
    let responses: Vec<JsonRpcResponse> = outcome
        .outgoing
        .iter()
        .filter_map(|message| match message {
            JsonRpcMessage::Response(response) => Some(response.clone()),
            _ => None,
        })
        .collect();
    assert_ok_response(response_for(&responses, 1, label), label);
    assert_ok_response(response_for(&responses, 41, label), label);
    let catalog_index = outcome
        .outgoing
        .iter()
        .position(|message| {
            matches!(message, JsonRpcMessage::Response(response)
                if response.id == JsonRpcRequest::new("probe", None, 41_i64).id)
        })
        .expect("tools/list was answered");
    assert!(
        acknowledgements[0] < catalog_index,
        "{label}: {:?}",
        outcome.outgoing
    );
    responses
}

/// POSITIVE: the matching cancellation is read while the listen is open and
/// retires it, so the listen gets no terminal response.
#[test]
#[cfg(not(feature = "legacy-2024-11-05"))]
fn srv_65_unsplit_listen_reads_its_cancellation_and_keeps_serving() {
    let label = "unsplit-listen-matching-cancel";
    let outcome = run_unsplit_listen_scenario(label, 40);
    let responses = assert_unsplit_listen_kept_reading(&outcome, label);
    let listen_id = JsonRpcRequest::new("probe", None, 40_i64).id;
    assert!(
        responses.iter().all(|response| response.id != listen_id),
        "{label}: a peer-cancelled listen must not be answered: {responses:?}"
    );
}

/// NEGATIVE: the same script, but the cancellation names another request. The
/// listen must NOT be retired by it; it stays open until EOF, where shutdown
/// completes it gracefully.
#[test]
#[cfg(not(feature = "legacy-2024-11-05"))]
fn srv_65_unsplit_listen_ignores_an_unrelated_cancellation_until_eof() {
    let label = "unsplit-listen-unrelated-cancel";
    let outcome = run_unsplit_listen_scenario(label, 999);
    let responses = assert_unsplit_listen_kept_reading(&outcome, label);
    let completion = response_for(&responses, 40, label);
    assert_ok_response(completion, label);
    let result = completion.result.as_ref().expect("completion result");
    assert_eq!(result["resultType"], "complete", "{label}: {completion:?}");
    assert_eq!(
        result["_meta"]["io.modelcontextprotocol/subscriptionId"], 40,
        "{label}: {completion:?}"
    );
}

#[test]
#[cfg(not(feature = "legacy-2024-11-05"))]
fn srv_65_modern_child_task_cancellation_preserves_connection_and_quiesces() {
    let mut wire = WireScenario::start_with_dispatch(true);
    wire.send(modern_request(
        "tools/call",
        60,
        Some(serde_json::json!({
            "name": "pending",
            "arguments": {
                "value": wire.subject,
            },
        })),
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while wire.control.entered.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

    // Concurrent sibling request is processed and answered while request 60 is active.
    wire.send(modern_request("tools/list", 61, None));
    let sibling = wire.response(61);
    assert_ok_response(&sibling, "sibling tools/list response");
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

    // Cancel the dispatch child after its response is written. This boundary
    // bypasses the handler's cancellation checkpoint and exercises the join
    // classification, while preserving the already committed response.
    wire.cancel_response_child(false);
    wire.control.released.store(true, Ordering::Release);
    let response = wire.response(60);
    assert_ok_response(&response, "response committed before child cancellation");

    // The connection and pump remain responsive: subsequent requests succeed.
    wire.send(modern_request("tools/list", 62, None));
    let subsequent = wire.response(62);
    assert_ok_response(&subsequent, "subsequent tools/list response");

    // Clean finish proves shutdown hook ordering and quiescence:
    // - returning split transport returns Ok(())
    // - on_shutdown hook ran after child cleanup
    // - no active child work survives after return
    wire.finish();
    assert_eq!(wire.send_control.cancelled.load(Ordering::Acquire), 1);
    assert_eq!(
        wire.control.active.load(Ordering::Acquire),
        0,
        "no child work survives after return"
    );
}

#[test]
#[cfg(not(feature = "legacy-2024-11-05"))]
fn srv_65_modern_child_task_cancellation_during_shutdown_quiesces() {
    let mut wire = WireScenario::start_with_dispatch(true);
    wire.send(modern_request(
        "tools/call",
        80,
        Some(serde_json::json!({
            "name": "pending",
            "arguments": {
                "value": wire.subject,
            },
        })),
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while wire.control.entered.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

    // Close input while the handler is pending so shutdown joins the child.
    wire.close_input();
    wire.cancel_response_child(false);
    wire.control.released.store(true, Ordering::Release);

    let response = wire.response(80);
    assert_ok_response(&response, "response committed during shutdown drain");

    let result = wire
        .outcome
        .recv_timeout(SCENARIO_DEADLINE)
        .expect("pump must settle during shutdown drain");
    wire.worker.take().unwrap().join().unwrap();
    assert!(result.is_ok(), "returning pump must succeed: {result:?}");
    assert_eq!(wire.send_control.cancelled.load(Ordering::Acquire), 1);
    assert!(wire.shutdown.load(Ordering::Acquire));
    assert_eq!(
        wire.shutdown_active.load(Ordering::Acquire),
        0,
        "shutdown hook must run after child quiescence"
    );
    assert_eq!(
        wire.control.active.load(Ordering::Acquire),
        0,
        "no child work survives after return"
    );
}

#[test]
#[cfg(not(feature = "legacy-2024-11-05"))]
fn srv_65_modern_child_task_cancellation_with_failed_write_terminates_connection() {
    let mut wire = WireScenario::start_with_dispatch(true);
    wire.send(modern_request(
        "tools/call",
        70,
        Some(serde_json::json!({
            "name": "pending",
            "arguments": {
                "value": wire.subject,
            },
        })),
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while wire.control.entered.load(Ordering::Acquire) == 0 && std::time::Instant::now() < deadline
    {
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

    // Sibling request succeeds and is received by the client before failure injection.
    wire.send(modern_request("tools/list", 71, None));
    let sibling = wire.response(71);
    assert_ok_response(&sibling, "sibling tools/list response");
    assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

    // As in the shutdown positive, EOF releases this custom transport's
    // blocking read. This case proves failure classification during drain;
    // it does not claim the server can interrupt an arbitrary blocking recv.
    wire.close_input();
    // Change the response write to failure at the same cancellation boundary.
    // ModernDispatchReservation::Drop must preserve that failure even though
    // the child also returns JoinError::Cancelled.
    wire.cancel_response_child(true);
    wire.control.released.store(true, Ordering::Release);

    let outcome = wire
        .outcome
        .recv_timeout(SCENARIO_DEADLINE)
        .expect("pump must settle after failed response write");
    wire.worker.take().unwrap().join().unwrap();
    assert_eq!(wire.send_control.cancelled.load(Ordering::Acquire), 1);
    let error = outcome.expect_err("connection MUST fail when response write fails");
    assert_eq!(error.code, fastmcp_core::McpErrorCode::InternalError);
    assert_eq!(
        error
            .data
            .as_ref()
            .and_then(|data| data.get("kind"))
            .and_then(|r| r.as_str()),
        Some("pump_failure"),
        "error kind must be pump_failure"
    );
    assert_eq!(
        wire.control.active.load(Ordering::Acquire),
        0,
        "no child work survives after return"
    );
}

#[cfg(not(feature = "legacy-2024-11-05"))]
mod post_receive_failure {
    use super::*;

    #[derive(Default)]
    struct FenceControl {
        receive_calls: AtomicUsize,
        received_frames: AtomicUsize,
        receive_timeouts: AtomicUsize,
        receive_closes: AtomicUsize,
        send_closes: AtomicUsize,
        write_attempts: AtomicUsize,
        progress_attempts: AtomicUsize,
        successful_writes: AtomicUsize,
        first_terminal_returning: AtomicBool,
        authentications: AtomicUsize,
    }

    struct ProgressPendingTool(PendingTool);

    impl ToolHandler for ProgressPendingTool {
        fn execution_mode(&self) -> fastmcp_server::ToolExecutionMode {
            self.0.execution_mode()
        }

        fn definition(&self) -> Tool {
            self.0.definition()
        }

        fn call(&self, ctx: &McpContext, arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            self.0.call(ctx, arguments)
        }

        fn call_final_outcome_async<'a>(
            &'a self,
            ctx: &'a McpContext,
            arguments: serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = McpOutcome<FinalToolOutcome>> + Send + 'a>,
        > {
            Box::pin(async move {
                let outcome = self.0.call_final_outcome_async(ctx, arguments).await;
                ctx.report_progress(0.5, Some("first request ready"));
                outcome
            })
        }
    }

    struct FenceAuth(Arc<FenceControl>);

    impl fastmcp_server::AuthProvider for FenceAuth {
        fn authenticate(
            &self,
            _ctx: &McpContext,
            _request: fastmcp_server::AuthRequest<'_>,
        ) -> McpResult<fastmcp_core::AuthContext> {
            self.0.authentications.fetch_add(1, Ordering::AcqRel);
            Ok(fastmcp_core::AuthContext::anonymous())
        }
    }

    struct FenceRecv(WireRecv, Arc<FenceControl>);

    impl TransportRecvHalf for FenceRecv {
        fn recv(&mut self, cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
            self.1.receive_calls.fetch_add(1, Ordering::Release);
            let result = self.0.recv(cx);
            match &result {
                Ok(_) => {
                    self.1.received_frames.fetch_add(1, Ordering::Release);
                }
                Err(TransportError::Io(error))
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
                    ) =>
                {
                    self.1.receive_timeouts.fetch_add(1, Ordering::Release);
                }
                _ => {}
            }
            result
        }

        fn close(&mut self, cx: &Cx) -> Result<(), TransportError> {
            self.1.receive_closes.fetch_add(1, Ordering::Release);
            self.0.close(cx)
        }
    }

    struct FenceSend {
        stream: TcpStream,
        control: Arc<FenceControl>,
        fail_progress_write: bool,
    }

    impl TransportSendHalf for FenceSend {
        fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
            self.control.write_attempts.fetch_add(1, Ordering::AcqRel);
            let progress = matches!(message, JsonRpcMessage::Request(notification)
                if notification.method == "notifications/progress");
            if progress {
                self.control
                    .progress_attempts
                    .fetch_add(1, Ordering::Release);
            }
            let result = if progress && self.fail_progress_write {
                Err(TransportError::Io(std::io::Error::from(
                    std::io::ErrorKind::BrokenPipe,
                )))
            } else {
                write_wire(&mut self.stream, message)
            };
            if result.is_ok() {
                self.control
                    .successful_writes
                    .fetch_add(1, Ordering::Release);
            }
            if result.is_ok()
                && matches!(message, JsonRpcMessage::Response(response)
                    if response.id == Some(90_i64.into()))
            {
                self.control
                    .first_terminal_returning
                    .store(true, Ordering::Release);
            }
            result
        }

        fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
            self.control.send_closes.fetch_add(1, Ordering::Release);
            self.stream.shutdown(Shutdown::Write)?;
            Ok(())
        }
    }

    fn start(fail_progress_write: bool) -> (WireScenario, Arc<FenceControl>, mpsc::Receiver<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (server_socket, peer) = listener.accept().unwrap();
        for stream in [&writer, &server_socket] {
            stream.set_read_timeout(Some(SCENARIO_DEADLINE)).unwrap();
            stream
                .set_write_timeout(Some(Duration::from_secs(3)))
                .unwrap();
        }
        let reader = BufReader::new(writer.try_clone().unwrap());
        let control = Arc::new(PendingControl::default());
        let fence = Arc::new(FenceControl::default());
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_active = Arc::new(AtomicUsize::new(usize::MAX));
        let shutdown_probe = Arc::clone(&shutdown);
        let shutdown_active_probe = Arc::clone(&shutdown_active);
        let shutdown_control = Arc::clone(&control);
        let server = Server::new("post-receive-failure", "1.0.0")
            .tool(ProgressPendingTool(PendingTool(Arc::clone(&control))))
            .auth_provider(FenceAuth(Arc::clone(&fence)))
            .on_shutdown(move || {
                shutdown_active_probe.store(
                    shutdown_control.active.load(Ordering::Acquire),
                    Ordering::Release,
                );
                shutdown_probe.store(true, Ordering::Release);
            })
            .build();
        let recv = FenceRecv(
            WireRecv(BufReader::new(server_socket.try_clone().unwrap())),
            Arc::clone(&fence),
        );
        let send = FenceSend {
            stream: server_socket,
            control: Arc::clone(&fence),
            fail_progress_write,
        };
        let barrier_control = Arc::clone(&fence);
        let barrier_shutdown = Arc::clone(&shutdown);
        let (settled_tx, settled_rx) = mpsc::channel();
        let (outcome_tx, outcome) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let runtime = asupersync::runtime::RuntimeBuilder::current_thread()
                .blocking_threads(0, 2)
                .build()
                .unwrap();
            let result = runtime.block_on(async move {
                let root = Cx::current().unwrap();
                let mut settled = root
                    .spawn(move |barrier_cx| async move {
                        while !barrier_control
                            .first_terminal_returning
                            .load(Ordering::Acquire)
                        {
                            if barrier_shutdown.load(Ordering::Acquire) {
                                return;
                            }
                            asupersync::time::sleep(barrier_cx.now(), Duration::from_millis(1))
                                .await;
                        }
                        // This task shares the sole async worker with request
                        // dispatch. It cannot observe the writer's marker until
                        // that dispatch poll returns and drops its reservation:
                        // there is no await between the writer and that drop.
                        let _ = settled_tx.send(());
                    })
                    .expect("the caller owns the dispatch-completion barrier");
                let dispatch_cx = root.clone();
                let mut pump = root
                    .spawn_blocking(move |pump_cx| {
                        server.run_split_transport_returning_with_dispatch_cx(
                            &pump_cx,
                            &dispatch_cx,
                            recv,
                            send,
                        )
                    })
                    .expect("the caller owns the blocking receive pump");
                let result = pump.join(&root).await.expect("pump must join normally");
                settled
                    .join(&root)
                    .await
                    .expect("barrier must join normally");
                result
            });
            let _ = outcome_tx.send(result);
        });
        (
            WireScenario {
                reader,
                writer,
                control,
                send_control: Arc::new(WireSendControl::default()),
                shutdown,
                shutdown_active,
                outcome,
                worker: Some(worker),
                subject: format!("post-receive-{peer}"),
                input_closed: false,
            },
            fence,
            settled_rx,
        )
    }

    fn run(fail_progress_write: bool) {
        let (mut wire, fence, settled) = start(fail_progress_write);
        let mut first = modern_request(
            "tools/call",
            90,
            Some(serde_json::json!({
                "name": "pending", "arguments": {"value": wire.subject},
            })),
        );
        first.params.as_mut().unwrap()["_meta"]["progressToken"] = serde_json::json!(wire.subject);
        wire.send(first);
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while (fence.receive_calls.load(Ordering::Acquire) < 2
            || wire.control.entered.load(Ordering::Acquire) < 1)
            && std::time::Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(fence.receive_calls.load(Ordering::Acquire), 2);
        assert_eq!(fence.received_frames.load(Ordering::Acquire), 1);
        assert_eq!(fence.write_attempts.load(Ordering::Acquire), 0);
        assert_eq!(fence.authentications.load(Ordering::Acquire), 1);
        assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
        assert_eq!(wire.control.active.load(Ordering::Acquire), 1);

        wire.control.released.store(true, Ordering::Release);
        settled
            .recv_timeout(SCENARIO_DEADLINE)
            .expect("first dispatch must settle while the second receive is blocked");
        assert_eq!(wire.control.active.load(Ordering::Acquire), 0);
        assert_eq!(fence.receive_calls.load(Ordering::Acquire), 2);
        assert_eq!(fence.received_frames.load(Ordering::Acquire), 1);
        assert_eq!(fence.write_attempts.load(Ordering::Acquire), 2);
        assert_eq!(fence.progress_attempts.load(Ordering::Acquire), 1);

        wire.send(modern_request(
            "tools/call",
            91,
            Some(serde_json::json!({
                "name": "pending", "arguments": {"value": wire.subject},
            })),
        ));
        wire.close_input();
        let result = wire
            .outcome
            .recv_timeout(SCENARIO_DEADLINE)
            .expect("the pump must return before harness cleanup");
        wire.worker.take().unwrap().join().unwrap();
        let mut responses = Vec::new();
        let mut notifications = Vec::new();
        loop {
            match read_wire(&mut wire.reader) {
                Ok(JsonRpcMessage::Response(response)) => responses.push(response),
                Ok(JsonRpcMessage::Request(notification)) => notifications.push(notification),
                Err(TransportError::Closed) => break,
                other => panic!("expected a correlated response or clean EOF, got {other:?}"),
            }
        }
        assert_eq!(fence.received_frames.load(Ordering::Acquire), 2);
        assert_eq!(fence.receive_timeouts.load(Ordering::Acquire), 0);
        assert_eq!(fence.receive_closes.load(Ordering::Acquire), 1);
        assert_eq!(fence.send_closes.load(Ordering::Acquire), 1);
        assert_eq!(wire.control.active.load(Ordering::Acquire), 0);
        assert!(wire.shutdown.load(Ordering::Acquire));
        assert_eq!(wire.shutdown_active.load(Ordering::Acquire), 0);
        assert_eq!(fence.progress_attempts.load(Ordering::Acquire), 1);
        if fail_progress_write {
            let error = result.expect_err("a settled write failure must fail the connection");
            assert_eq!(error.code, fastmcp_core::McpErrorCode::InternalError);
            assert_eq!(error.data.as_ref().unwrap()["kind"], "pump_failure");
            assert_eq!(fence.authentications.load(Ordering::Acquire), 1);
            assert_eq!(wire.control.entered.load(Ordering::Acquire), 1);
            assert_eq!(fence.receive_calls.load(Ordering::Acquire), 2);
            assert_eq!(fence.write_attempts.load(Ordering::Acquire), 2);
            assert_eq!(fence.successful_writes.load(Ordering::Acquire), 1);
            assert!(notifications.is_empty());
            assert_eq!(responses.len(), 1);
            assert_eq!(responses[0].id, Some(90_i64.into()));
            // This handler's progress stays queued until finalization has
            // elected Complete. Its failed flush must fail the connection,
            // but cannot replace that elected result with cancellation.
            assert_ok_response(&responses[0], "completion elected before progress flush");
            let terminal = responses[0].result.as_ref().expect("elected tool result");
            assert_eq!(terminal["resultType"], "complete");
            assert_eq!(
                terminal["content"],
                serde_json::json!([{"type": "text", "text": wire.subject}])
            );
        } else {
            result.expect("successful output must permit the next request");
            assert_eq!(fence.authentications.load(Ordering::Acquire), 2);
            assert_eq!(wire.control.entered.load(Ordering::Acquire), 2);
            assert_eq!(fence.receive_calls.load(Ordering::Acquire), 3);
            assert_eq!(fence.write_attempts.load(Ordering::Acquire), 3);
            assert_eq!(fence.successful_writes.load(Ordering::Acquire), 3);
            assert_eq!(notifications.len(), 1);
            assert_eq!(notifications[0].method, "notifications/progress");
            assert_eq!(
                notifications[0].params.as_ref().unwrap()["progressToken"],
                wire.subject
            );
            assert_eq!(responses.len(), 2);
            for (response, id) in responses.iter().zip([90_i64, 91]) {
                assert_eq!(response.id, Some(id.into()));
                assert_ok_response(response, "request before/after successful write");
                assert_eq!(
                    response.result.as_ref().unwrap()["content"][0]["text"],
                    wire.subject
                );
            }
        }
    }

    #[test]
    fn fnd_04_b_frame_after_successful_write_dispatches() {
        run(false);
    }

    #[test]
    fn fnd_04_b_frame_after_failed_write_is_not_admitted() {
        run(true);
    }
}

/// bd-6rfrg's sampling fixture, which can only exist in the legacy era.
///
/// Gated because `initialize` -- the only frame that tells this transport the
/// client supports sampling -- is a legacy-era opener, so under
/// `--no-default-features` there is no era in which these arms can run. The
/// trait asymmetry under test is era-independent; the ability to ADVERTISE the
/// capability on an in-process transport is not.
#[cfg(feature = "legacy-2024-11-05")]
mod bd_6rfrg_sampling_bridge {
    use super::*;

    // ===========================================================================
    // bd-6rfrg G1: the sampling hazard, reproduced through the PUBLIC trait.
    //
    // `ToolHandler::call` is REQUIRED and synchronous (handler.rs:1666) and its own
    // doc comment calls it "the default implementation point. Override this for
    // simple synchronous tools." (handler.rs:1663), while `McpContext::sample` is
    // `async` (context.rs:3534). A user who follows the required method and the
    // documentation has to bridge the two, and `fastmcp_core::block_on` is the
    // bridge this workspace exports. Nothing in the trait, the doc, or the compiler
    // tells them not to.
    //
    // The two handlers below carry the SAME body and differ in exactly one
    // property: which trait method it hangs off. `SyncSamplingTool` implements only
    // the required sync `call` and bridges with `block_on`, leaving every async hook
    // at its default. `AsyncSamplingTool` overrides the async final hook and awaits
    // `ctx.sample` directly. Both answer through the same server, the same scripted
    // transport, and the same result construction (`Echo`), so a difference between
    // their outcomes is a property of the API shape and not of the fixture.
    //
    // THE ASYNC ARM IS THE CONTROL, AND IT EXISTS TO MAKE A REFUTATION POSSIBLE.
    // `sample_with_request` returns an error IMMEDIATELY when no sampling sender is
    // configured (context.rs:3580), and an early error is neither a deadlock nor
    // evidence against one. So a sync arm that does not complete means nothing
    // unless the async arm, in the same fixture, completes with the sampled text.
    // `AsyncSamplingTool::call` therefore refuses rather than delegating: if the
    // router ever reaches the control through the sync bridge, the control fails
    // loudly instead of passing for the wrong reason.
    // ===========================================================================

    /// The prompt the fixture's handlers sample with.
    const SAMPLING_PROMPT: &str = "bd-6rfrg: does a sync handler get its completion?";

    /// What the scripted client answers `sampling/createMessage` with. Distinct
    /// from [`SAMPLING_PROMPT`] so a returned value proves a ROUND TRIP happened
    /// rather than an echo of the request.
    const SAMPLED_TEXT: &str = "bd-6rfrg-sampled-completion";

    /// Wall-clock grace the sampling transport keeps `recv` alive after its script
    /// drains, so the server's reverse `sampling/createMessage` request has
    /// somewhere to be answered. Bounded: `recv` reports `Closed` at the end of it
    /// whatever happened, so the pump can never park in `recv` forever.
    const SAMPLING_RECV_GRACE: Duration = Duration::from_secs(5);

    /// Bound on one sampling scenario, deliberately shorter than
    /// [`SCENARIO_DEADLINE`] because the hazard arm is EXPECTED to consume it.
    ///
    /// It is self-validating rather than guessed: the control arm runs under this
    /// same bound and a healthy sampling round trip finishes in milliseconds, so a
    /// control that completes proves the bound was adequate on the machine that
    /// produced the run. A bound that only ever fires is indistinguishable from a
    /// bound that is too short.
    const SAMPLING_DEADLINE: Duration = Duration::from_secs(20);

    /// Which position the fixture exercises.
    ///
    /// These form a LADDER, and a rung means nothing unless the rung below it
    /// passed in the same run: `PlainEcho` establishes that the harness answers a
    /// `tools/call` at all, `DeclaredAsync` that sampling is live through it, and
    /// only then does `RequiredSyncCall` say anything about the trait positions.
    /// The first version of this fixture had no `PlainEcho`, and all three of its
    /// positions returned the same `NoCorrelatedResponse` -- a harness failure
    /// wearing the costume of a sampling result.
    #[derive(Clone, Copy, Debug)]
    enum SamplingArm {
        /// `echo`, which samples nothing. Proves the harness round-trips a call.
        PlainEcho,
        /// The required sync `call`, bridging `ctx.sample` with `block_on`.
        RequiredSyncCall,
        /// The async final hook, awaiting `ctx.sample` directly.
        DeclaredAsync,
    }

    /// What one arm did, inside the bound. Recorded rather than asserted, so the
    /// reproduction reports a refutation as readily as a confirmation.
    #[derive(Debug)]
    enum SamplingOutcome {
        /// The tool answered. Carries the text it returned.
        Completed(String),
        /// The tool answered with a JSON-RPC error. Carries its message.
        Errored(String),
        /// Nothing came back inside [`SAMPLING_DEADLINE`]. This is the hazard: no
        /// error, no timeout, no diagnostic, and the evidence destroyed with it.
        NoOutcomeWithinBound {
            #[allow(
                dead_code,
                reason = "read only through the derived Debug in the arms' `{subject:?}` failure \
                          messages; dead-code analysis ignores derived Debug by design"
            )]
            sampling_requests: usize,
        },
        /// The pump returned but produced no response correlated to the call.
        NoCorrelatedResponse {
            observed: Vec<Option<fastmcp_protocol::RequestId>>,
        },
    }

    #[derive(Default)]
    struct SamplingScriptedState {
        incoming: VecDeque<JsonRpcMessage>,
        outgoing: Vec<JsonRpcMessage>,
        sampling_requests: usize,
    }

    /// A scripted transport that ANSWERS the server's reverse sampling request
    /// instead of only recording it, so `ctx.sample` can actually complete.
    struct SamplingScriptedTransport {
        state: Arc<Mutex<SamplingScriptedState>>,
    }

    #[derive(Clone)]
    struct SamplingScriptedProbe(Arc<Mutex<SamplingScriptedState>>);

    impl SamplingScriptedProbe {
        fn responses(&self) -> Vec<JsonRpcResponse> {
            self.0
                .lock()
                .expect("sampling transport mutex must not be poisoned")
                .outgoing
                .iter()
                .filter_map(|message| match message {
                    JsonRpcMessage::Response(response) => Some(response.clone()),
                    _ => None,
                })
                .collect()
        }

        fn sampling_requests(&self) -> usize {
            self.0
                .lock()
                .expect("sampling transport mutex must not be poisoned")
                .sampling_requests
        }
    }

    impl Transport for SamplingScriptedTransport {
        fn send(&mut self, _cx: &Cx, message: &JsonRpcMessage) -> Result<(), TransportError> {
            let mut state = self
                .state
                .lock()
                .expect("sampling transport mutex must not be poisoned");
            state.outgoing.push(message.clone());
            // Answer the reverse request the same way a sampling-capable client
            // would: same id, assistant role, one text block. Built through
            // `CreateMessageResult` rather than a hand-written object so the member
            // names and the content discriminator come from the protocol crate.
            if let JsonRpcMessage::Request(request) = message
                && request.method == "sampling/createMessage"
                && let Some(id) = request.id.clone()
            {
                state.sampling_requests += 1;
                let result = serde_json::to_value(fastmcp_protocol::CreateMessageResult::text(
                    SAMPLED_TEXT,
                    "bd-6rfrg-fixture-model",
                ))
                .expect("a CreateMessageResult serializes to JSON");
                state
                    .incoming
                    .push_back(JsonRpcMessage::Response(JsonRpcResponse::success(
                        id, result,
                    )));
            }
            Ok(())
        }

        fn recv(&mut self, _cx: &Cx) -> Result<JsonRpcMessage, TransportError> {
            let deadline = std::time::Instant::now() + SAMPLING_RECV_GRACE;
            loop {
                let next = self
                    .state
                    .lock()
                    .expect("sampling transport mutex must not be poisoned")
                    .incoming
                    .pop_front();
                if let Some(message) = next {
                    return Ok(message);
                }
                if std::time::Instant::now() >= deadline {
                    return Err(TransportError::Closed);
                }
                // The lock is released before this sleep: a handler answering a
                // reverse request must be able to reach `send` while `recv` waits.
                std::thread::sleep(Duration::from_millis(1));
            }
        }

        fn close(&mut self, _cx: &Cx) -> Result<(), TransportError> {
            Ok(())
        }
    }

    /// The `Tool` both arms advertise, differing only in name. Reuses `Echo`'s
    /// schema so argument validation cannot become the reason an arm fails.
    fn sampling_tool_definition(name: &str) -> Tool {
        let mut definition = Echo.definition();
        definition.name = name.to_owned();
        definition.description =
            Some("bd-6rfrg fixture: requests a completion from inside the handler".to_owned());
        definition
    }

    /// The obvious sync tool that samples. Overrides NOTHING else, which is the
    /// whole point: the user picked no execution mode and implemented the one
    /// method the trait requires.
    struct SyncSamplingTool;

    impl ToolHandler for SyncSamplingTool {
        fn definition(&self) -> Tool {
            sampling_tool_definition("sync_sample")
        }

        fn call(&self, ctx: &McpContext, _arguments: serde_json::Value) -> McpResult<Vec<Content>> {
            // The bridge a user reaches for, because `call` is sync and `sample`
            // is not. Returning through `Echo` keeps this arm's result shape
            // identical to the control's.
            let response = block_on(ctx.sample(SAMPLING_PROMPT, 16))?;
            Echo.call(ctx, serde_json::json!({ "value": response.text }))
        }
    }

    /// The same body reached through the async hook. The control.
    struct AsyncSamplingTool;

    impl ToolHandler for AsyncSamplingTool {
        fn execution_mode(&self) -> fastmcp_server::ToolExecutionMode {
            fastmcp_server::ToolExecutionMode::Async
        }

        fn definition(&self) -> Tool {
            sampling_tool_definition("async_sample")
        }

        /// Refuses instead of delegating. If the router reaches the control through
        /// the sync bridge, the control must fail loudly rather than pass for the
        /// wrong reason.
        fn call(
            &self,
            _ctx: &McpContext,
            _arguments: serde_json::Value,
        ) -> McpResult<Vec<Content>> {
            Err(fastmcp_core::McpError::internal_error(
                "bd-6rfrg control reached through the SYNC path; the control proves nothing",
            ))
        }

        fn call_final_outcome_async<'a>(
            &'a self,
            ctx: &'a McpContext,
            _arguments: serde_json::Value,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = McpOutcome<FinalToolOutcome>> + Send + 'a>,
        > {
            Box::pin(async move {
                let sampled = match ctx.sample(SAMPLING_PROMPT, 16).await {
                    Ok(response) => response.text,
                    Err(error) => return McpOutcome::Err(error),
                };
                match Echo.call_final(ctx, serde_json::json!({ "value": sampled })) {
                    Ok(result) => McpOutcome::Ok(FinalToolOutcome::Complete(result)),
                    Err(error) => McpOutcome::Err(error),
                }
            })
        }
    }

    /// The request sequence, in the LEGACY 2024-11-05 ERA, and that is forced.
    ///
    /// WHY NOT THE MODERN `_meta` ENVELOPE, WHICH I TRIED SECOND: on this transport
    /// the sampling sender is installed from `session.supports_sampling()`
    /// (session.rs:537), which reads capabilities stored by the `initialize`
    /// handshake (session.rs:725 proves that is what stores them). The only
    /// sender-construction site this loop reaches is session-based:
    /// `create_bidirectional_senders(session, ..)` in lib.rs. The reader that
    /// consumes `_meta` client capabilities, `admitted_final_client_capability_info`
    /// (lib.rs:1865), is called from `serve_modern_http_connection` and NOWHERE
    /// ELSE. So a modern `_meta` frame on this path cannot advertise sampling at
    /// all, and my second attempt got the capability refusal
    /// (context.rs `sample_with_request`) instead of a sampling result.
    ///
    /// AND WHY MY FIRST ATTEMPT FAILED, WHICH IS NOW EXACTLY DIAGNOSABLE: it sent a
    /// legacy `initialize` AND THEN put the modern `_meta` envelope on the
    /// `tools/call`. The opening frame selects the era (`classify_opening`,
    /// lib.rs:17254; `srv_02_b` proves a stdio `initialize` is rejected outright
    /// under `ModernOnly`, so `initialize` IS the legacy opener). Frame 1 selected
    /// legacy, frame 3 arrived modern, admission refused it, and the run answered
    /// id 1 and nothing after. The defect was MIXING ERAS, not the handshake.
    ///
    /// So: one era throughout. Initialize params copied from `leg_02_b_contract`'s
    /// proven `initialize_wire`, plus the `initialized` notification it also sends,
    /// and a `tools/call` carrying NO `_meta`.
    fn sampling_request_script(tool: &str) -> Vec<JsonRpcRequest> {
        vec![
            JsonRpcRequest::new(
                "initialize",
                Some(serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"sampling": {}},
                    "clientInfo": {"name": "bd-6rfrg-sampling-probe", "version": "1.0.0"},
                })),
                1_i64,
            ),
            JsonRpcRequest::notification("notifications/initialized", None),
            JsonRpcRequest::new(
                "tools/call",
                Some(serde_json::json!({
                    "name": tool,
                    "arguments": {"value": "unused"},
                })),
                3_i64,
            ),
        ]
    }

    /// Refuses to let an arm be read as a sampling result when the harness never
    /// answered the call.
    fn require_the_harness_answered(outcome: &SamplingOutcome, arm: &str) {
        if let SamplingOutcome::NoCorrelatedResponse { observed } = outcome {
            panic!(
                "[{arm}] THE HARNESS DID NOT ANSWER THE CALL AT ALL, so this arm measures nothing \
                 about the trait positions. Run \
                 bd_6rfrg_the_harness_answers_a_plain_tool_call first: if that fails too, the \
                 fixture is broken and NO conclusion about sampling is available from this run. \
                 Response ids observed: {observed:?}"
            );
        }
    }

    /// Drives one arm under an external bound and RECORDS what happened.
    ///
    /// The bound is a real wall clock on the test thread, outside the server's
    /// runtime and outside asupersync's timer, because a receipt for a hang cannot
    /// be produced by a process that hung. On expiry the worker thread is
    /// deliberately NOT joined: it is the thread that is stuck, and joining it
    /// would move the hang into the assertion.
    fn run_sampling_arm(arm: SamplingArm) -> SamplingOutcome {
        let tool_name = match arm {
            // Same server as the sync arm; the ONLY difference is which tool the
            // call names, so a PlainEcho failure cannot be blamed on registration.
            SamplingArm::PlainEcho => "echo",
            SamplingArm::RequiredSyncCall => "sync_sample",
            SamplingArm::DeclaredAsync => "async_sample",
        };
        let state = Arc::new(Mutex::new(SamplingScriptedState {
            incoming: sampling_request_script(tool_name)
                .into_iter()
                .map(JsonRpcMessage::Request)
                .collect(),
            ..SamplingScriptedState::default()
        }));
        let probe = SamplingScriptedProbe(Arc::clone(&state));
        let transport = SamplingScriptedTransport {
            state: Arc::clone(&state),
        };
        let (tx, rx) = mpsc::channel();
        let probe_for_thread = probe.clone();
        let _worker = std::thread::Builder::new()
            .name(format!("bd-6rfrg-{tool_name}"))
            .spawn(move || {
                block_on(async move {
                    let cx = Cx::current().expect("the asupersync runtime installs a current Cx");
                    let server = match arm {
                        SamplingArm::PlainEcho | SamplingArm::RequiredSyncCall => {
                            Server::new("bd-6rfrg", "1.0.0").tool(SyncSamplingTool)
                        }
                        SamplingArm::DeclaredAsync => {
                            Server::new("bd-6rfrg", "1.0.0").tool(AsyncSamplingTool)
                        }
                    }
                    .tool(Echo)
                    .build();
                    // Keep the caller's current-thread executor free to drive
                    // request children while the blocking pool owns the sync
                    // transport loop, exactly as `run_scenario` above does.
                    if let Ok(mut pump) = cx.spawn_blocking(move |pump_cx| {
                        server.run_transport_returning_with_cx(&pump_cx, transport)
                    }) {
                        let _ = pump.join(&cx).await;
                    }
                });
                let _ = tx.send(probe_for_thread.responses());
            })
            .expect("the sampling arm worker thread must start");

        match rx.recv_timeout(SAMPLING_DEADLINE) {
            Ok(responses) => {
                let wanted = JsonRpcRequest::new("probe", None, 3_i64).id;
                match responses.iter().find(|response| response.id == wanted) {
                    Some(response) => match (&response.result, &response.error) {
                        (Some(result), _) => {
                            // A TOOL-LEVEL ERROR IS STILL A JSON-RPC `result`,
                            // carrying `isError: true` with the message as its
                            // content. Reading only `result` classified
                            // `Completed("Sampling not available: client does
                            // not support sampling capability")` as a success
                            // and hid the fact that the two arms had begun to
                            // diverge. A classifier that cannot tell its own
                            // failure mode from its success is the third one of
                            // those in this fixture.
                            let text = result["content"][0]["text"]
                                .as_str()
                                .unwrap_or("<no text content>")
                                .to_owned();
                            if result["isError"] == serde_json::Value::Bool(true) {
                                SamplingOutcome::Errored(text)
                            } else {
                                SamplingOutcome::Completed(text)
                            }
                        }
                        (None, Some(error)) => SamplingOutcome::Errored(error.message.clone()),
                        (None, None) => {
                            SamplingOutcome::Errored("<neither result nor error>".to_owned())
                        }
                    },
                    None => SamplingOutcome::NoCorrelatedResponse {
                        observed: responses
                            .iter()
                            .map(|response| response.id.clone())
                            .collect(),
                    },
                }
            }
            Err(_) => SamplingOutcome::NoOutcomeWithinBound {
                sampling_requests: probe.sampling_requests(),
            },
        }
    }

    /// bd-6rfrg, rung zero. The harness answers a `tools/call` at all.
    ///
    /// Nothing above this rung is interpretable without it. The first run of this
    /// fixture returned `NoCorrelatedResponse { observed: [Some(Number(1))] }` for
    /// ALL THREE positions -- control, subject and negative alike -- which is a
    /// transport-correlation failure occurring before any handler runs, not a
    /// result about sampling. This test exists so that failure reports itself here
    /// instead of being read one rung up.
    #[test]
    fn bd_6rfrg_the_harness_answers_a_plain_tool_call() {
        let outcome = run_sampling_arm(SamplingArm::PlainEcho);
        println!("bd-6rfrg rung 0: {outcome:?}");
        match &outcome {
            SamplingOutcome::Completed(text) => assert_eq!(
                text, "unused",
                "the harness must round-trip the argument it sent, so a later arm's \
                 `Completed` can be trusted to mean the handler ran"
            ),
            other => panic!(
                "THE FIXTURE IS BROKEN, NOT THE SUBJECT: a tool that samples nothing did not \
                 complete, so no bd-6rfrg arm in this run says anything about the trait \
                 positions: {other:?}"
            ),
        }
    }

    /// bd-6rfrg G1, the control. Sampling must be LIVE in this fixture, or the
    /// sync arm's outcome is uninterpretable in either direction.
    ///
    /// This is also G5's planted negative for the test below: a near-identical
    /// handler, differing only in which trait method carries the body, that
    /// demonstrates the assertion there CAN come out the other way.
    #[test]
    #[ignore = "bd-6rfrg: this host structurally cannot sample. A hand-rolled in-memory Transport has no reverse-request sender, so `bidirectional_senders` returns None at lib.rs:1413 before capabilities are read (legacy era), and the `_meta` capability reader exists only in serve_modern_http_connection (modern era). The live fixtures are e2e_public_http_bd_6rfrg_sync_bridge_completes_the_sampling_round_trip and e2e_public_http_bd_6rfrg_driver_bridge_is_diagnosed_not_hung in crates/fastmcp/tests/e2e_modern_http.rs. Rung 0 below still runs and still asserts something true about THIS transport, so it is not ignored."]
    fn bd_6rfrg_sampling_is_live_when_the_handler_awaits_it_directly() {
        let outcome = run_sampling_arm(SamplingArm::DeclaredAsync);
        require_the_harness_answered(&outcome, "control");
        match &outcome {
            SamplingOutcome::Completed(text) => assert_eq!(
                text, SAMPLED_TEXT,
                "the control must carry the text the scripted client SAMPLED, not an echo \
                 of the prompt: a round trip is what makes sampling 'live'"
            ),
            other => panic!(
                "bd-6rfrg CONTROL FAILED, so the hazard arm proves nothing in either \
                 direction: an async handler awaiting ctx.sample did not complete: {other:?}"
            ),
        }
    }

    /// bd-6rfrg G1, the subject. Reproduced through the public trait, not through
    /// bd-f2ndd's probe.
    ///
    /// THIS TEST ASSERTS THAT THE HAZARD EXISTS. Green means the same handler body
    /// that completes through the async hook does NOT complete through the required
    /// sync method. Red means the premise of bd-6rfrg is wrong, and the failure
    /// message is the refutation report rather than a reason to adjust the fixture
    /// until it hangs. Whoever removes the hazard must update this test
    /// deliberately; its name says what it characterises.
    ///
    /// Run with `--nocapture` to read which non-completing class occurred:
    /// `NoOutcomeWithinBound` is the silent hang the bead was filed for, while
    /// `Errored` would mean the failure is already nameable.
    #[test]
    #[ignore = "bd-6rfrg: this host structurally cannot sample. A hand-rolled in-memory Transport has no reverse-request sender, so `bidirectional_senders` returns None at lib.rs:1413 before capabilities are read (legacy era), and the `_meta` capability reader exists only in serve_modern_http_connection (modern era). The live fixtures are e2e_public_http_bd_6rfrg_sync_bridge_completes_the_sampling_round_trip and e2e_public_http_bd_6rfrg_driver_bridge_is_diagnosed_not_hung in crates/fastmcp/tests/e2e_modern_http.rs. Rung 0 below still runs and still asserts something true about THIS transport, so it is not ignored."]
    fn bd_6rfrg_the_required_sync_call_cannot_complete_the_same_sampling_body() {
        let outcome = run_sampling_arm(SamplingArm::RequiredSyncCall);
        println!("bd-6rfrg G1 sync arm: {outcome:?}");
        // WITHOUT THIS GUARD THE ASSERTION BELOW IS UNSOUND: `NoCorrelatedResponse`
        // is not `Completed`, so a harness that answered nothing would have read as
        // the hazard CONFIRMED. The negative arm of the G5 test is what exposed it.
        require_the_harness_answered(&outcome, "G1 subject");
        // A COMPLETION IS ONLY A REFUTATION IF IT COMPLETED THE SAMPLING BODY.
        // The first version asserted merely `!Completed`, and when the arm DID
        // complete it reported a refutation -- on a value that was
        // `Completed("Sampling not available: client does not support sampling
        // capability")`. That is the early capability error, which this fixture's
        // own G1 rationale already excludes as evidence in either direction: a
        // bridge that is never entered cannot show that entering it is safe. So the
        // discriminator is the SAMPLED TEXT, which only a real round trip produces.
        if let SamplingOutcome::Completed(text) = &outcome {
            assert_eq!(
                text, SAMPLED_TEXT,
                "THE FIXTURE IS BROKEN, NOT THE SUBJECT: the sync arm completed without \
                 sampling anything, so it says nothing about the trait positions. A \
                 capability refusal here means the client never advertised sampling on \
                 this era's path. Completed with: {text:?}"
            );
            panic!(
                "PREMISE NOT REPRODUCED: the sync tool completed a REAL sampling round trip \
                 through the required `call` method, returning the sampled text. Report this \
                 as a finding about bd-6rfrg's premise; do not adjust the fixture until it \
                 hangs, and do not treat this test's failure as authority over the bead's \
                 disposition -- that is the orchestrator's call, not a test's."
            );
        }
    }

    /// Is `outcome` the bd-6rfrg diagnosis -- an error naming both the bridge and
    /// the way out of it? Two substrings rather than one because an error that
    /// names the problem without naming the remedy leaves the user exactly as stuck
    /// as the hang did, and G3 asks for something actionable, not merely audible.
    fn names_the_sampling_bridge(outcome: &SamplingOutcome) -> bool {
        match outcome {
            SamplingOutcome::Errored(message) => {
                message.contains("block_on") && message.contains("ToolExecutionMode::Async")
            }
            _ => false,
        }
    }

    /// bd-6rfrg G5. The decided behaviour of remedy (c1): the sync bridge is
    /// DIAGNOSED rather than silent.
    ///
    /// ORDER OF EVIDENCE MATTERS AND THIS TEST CANNOT ESTABLISH IT ALONE. Remedy
    /// (c1) converts the hang into this error, so a green run here is consistent
    /// with two different worlds: the hazard existed and was remedied, or this
    /// fixture's dispatch path was never trapped and the error is a false
    /// rejection. Only
    /// `bd_6rfrg_the_required_sync_call_cannot_complete_the_same_sampling_body`,
    /// RUN AT 911707e5 -- the commit before the remedy -- separates them. If that
    /// run reports `Completed`, this test is asserting a false rejection and the
    /// remedy must be reverted rather than this test kept.
    #[test]
    #[ignore = "bd-6rfrg: this host structurally cannot sample. A hand-rolled in-memory Transport has no reverse-request sender, so `bidirectional_senders` returns None at lib.rs:1413 before capabilities are read (legacy era), and the `_meta` capability reader exists only in serve_modern_http_connection (modern era). The live fixtures are e2e_public_http_bd_6rfrg_sync_bridge_completes_the_sampling_round_trip and e2e_public_http_bd_6rfrg_driver_bridge_is_diagnosed_not_hung in crates/fastmcp/tests/e2e_modern_http.rs. Rung 0 below still runs and still asserts something true about THIS transport, so it is not ignored."]
    fn bd_6rfrg_the_sync_sampling_bridge_is_diagnosed_not_silent() {
        let subject = run_sampling_arm(SamplingArm::RequiredSyncCall);
        let negative = run_sampling_arm(SamplingArm::DeclaredAsync);
        println!("bd-6rfrg G5 subject: {subject:?}");
        println!("bd-6rfrg G5 negative: {negative:?}");
        require_the_harness_answered(&subject, "G5 subject");
        require_the_harness_answered(&negative, "G5 negative");

        assert!(
            !matches!(subject, SamplingOutcome::NoOutcomeWithinBound { .. }),
            "G3: the detection did not fire and the request went silent again, which is the \
             one outcome no remedy may leave in place: {subject:?}"
        );
        assert!(
            names_the_sampling_bridge(&subject),
            "G5: the sync bridge must be diagnosed by an error naming both block_on and the \
             async hook that replaces it: {subject:?}"
        );
        // PLANTED NEGATIVE, RH-5. The SAME predicate, one trait method over. The
        // async hook completes, so it must NOT be diagnosed: an assertion that held
        // for both arms would be measuring the predicate's appetite rather than the
        // difference between the two positions.
        assert!(
            !names_the_sampling_bridge(&negative),
            "G5 NEGATIVE FAILED: the async arm was diagnosed as a starved bridge too, so the \
             predicate does not discriminate and the assertion above is worthless: {negative:?}"
        );
    }
}
