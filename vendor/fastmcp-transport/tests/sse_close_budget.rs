#![cfg(feature = "legacy-2024-11-05")]
// `SseWriter` exists only under this opt-in feature, so this proof compiles
// to nothing without it - matching `leg_http_01_a.rs`. Run it with
// `--features legacy-2024-11-05`; a default-feature run reports zero tests
// here, which is absence of the capability, not evidence about it.

//! B-03 capability proof: `SseWriter::close` CONSUMES the caller's budget.
//!
//! The second blocking-I/O close to observe a budget rather than merely receive
//! one, after `StdioTransport::close`. `SseWriter::close` flushes the write side,
//! and both `SseServerTransport::close` and `SseServerSendHalf::close` delegate
//! to it, so those two transports become budget-aware through this one change.
//!
//! These tests are an EXTERNAL CONSUMER: they construct the writer through the
//! public `fastmcp_transport::sse::SseWriter::new`, call the public `close`, and
//! assert only on public observables.
//!
//! WHY THIS PROOF IS STRONGER THAN THE STDIO ONE. `sse.rs` already guarded ten
//! I/O sites with `cx.is_cancel_requested()`, which observes the explicit
//! cancellation bit and nothing else. A caller whose DEADLINE has expired passes
//! that guard and proceeds into the blocking flush. `close` now uses
//! `sse_checkpoint`, mirroring the stdio and http helpers, so the expired-deadline
//! row below returns `Timeout` where the older idiom would have returned `Ok`
//! after doing the very I/O the caller had run out of budget for. That row is the
//! reason the checkpoint pattern was chosen over this module's local idiom.
//!
//! `Cx::for_testing*` is `test-internals` gated and this crate's dev-dependencies
//! enable it to CONSTRUCT a caller context. The API under test is public and
//! ships without that feature, so this is not the FND-04 B-13 trapdoor, which is
//! the reverse shape: reaching a production capability that exists only under the
//! feature.

use std::io::Write;
use std::sync::{Arc, Mutex};

use asupersync::Cx;
use fastmcp_transport::TransportError;
use fastmcp_transport::Transport;
use fastmcp_transport::sse::{SseClientTransport, SseWriter};

/// Records bytes written and how often the writer was flushed.
///
/// The flush count is the load-bearing observable: it separates "refused BEFORE
/// doing blocking I/O" from "did the I/O and reported an error afterwards",
/// which the returned error alone cannot distinguish.
#[derive(Clone, Default)]
struct CountingWriter {
    inner: Arc<Mutex<WriterState>>,
}

#[derive(Default)]
struct WriterState {
    bytes: Vec<u8>,
    flushes: usize,
}

impl CountingWriter {
    fn flushes(&self) -> usize {
        self.inner.lock().expect("writer mutex is uncontended").flushes
    }

    fn wrote_nothing(&self) -> bool {
        self.inner
            .lock()
            .expect("writer mutex is uncontended")
            .bytes
            .is_empty()
    }
}

impl Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.inner
            .lock()
            .expect("writer mutex is uncontended")
            .bytes
            .extend_from_slice(buffer);
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().expect("writer mutex is uncontended").flushes += 1;
        Ok(())
    }
}

fn cancelled_context() -> Cx {
    let cx = Cx::for_testing();
    cx.set_cancel_requested(true);
    cx
}

/// A context whose deadline has already passed. The cancellation bit is NEVER
/// set here, which is exactly what makes this row distinguish `checkpoint` from
/// `is_cancel_requested`.
fn expired_context() -> Cx {
    Cx::for_testing_with_budget(asupersync::Budget::new().with_deadline(asupersync::Time::ZERO))
}

mod ingress_budget {
    use super::{CountingWriter, cancelled_context, expired_context};
    use asupersync::Cx;
    use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest, JsonRpcResponse};
    use fastmcp_transport::sse::{
        LegacySseClientTransport, LegacySseMessagePost, LegacySsePostSink,
        SseClientTransport, SseEvent, SseReader, SseServerTransport,
    };
    use fastmcp_transport::{Transport, TransportError, TransportRecvHalf, TransportSendHalf};
    use std::io::{Cursor, Error, ErrorKind, Read};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct ReadEffects {
        calls: usize,
        bytes: usize,
    }

    #[derive(Default)]
    struct ReadControl {
        cancel_after_progress: Option<Cx>,
        cancel_before_progress: Option<Cx>,
        interrupt_once: bool,
    }

    struct ProbeReader {
        input: Cursor<Vec<u8>>,
        chunk: usize,
        effects: Arc<Mutex<ReadEffects>>,
        control: Arc<Mutex<ReadControl>>,
    }

    impl ProbeReader {
        fn new(input: Vec<u8>, chunk: usize) -> Self {
            assert!(chunk > 0);
            Self {
                input: Cursor::new(input),
                chunk,
                effects: Arc::new(Mutex::new(ReadEffects::default())),
                control: Arc::new(Mutex::new(ReadControl::default())),
            }
        }
    }

    impl Read for ProbeReader {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            self.effects.lock().expect("read effects").calls += 1;
            let mut control = self.control.lock().expect("read control");
            if let Some(cx) = control.cancel_before_progress.take() {
                cx.set_cancel_requested(true);
                return Err(Error::from(ErrorKind::Interrupted));
            }
            if std::mem::take(&mut control.interrupt_once) {
                return Err(Error::from(ErrorKind::Interrupted));
            }
            let limit = bytes.len().min(self.chunk);
            let read = self.input.read(&mut bytes[..limit])?;
            self.effects.lock().expect("read effects").bytes += read;
            if let Some(cx) = control.cancel_after_progress.take() {
                cx.set_cancel_requested(true);
            }
            Ok(read)
        }
    }

    fn snapshot(effects: &Arc<Mutex<ReadEffects>>) -> ReadEffects {
        effects.lock().expect("read effects").clone()
    }

    fn message_wire() -> Vec<u8> {
        SseEvent::message(r#"{"jsonrpc":"2.0","id":41,"result":{"ok":true}}"#)
            .to_bytes()
            .expect("response event")
    }

    fn assert_response(message: JsonRpcMessage) {
        let JsonRpcMessage::Response(response) = message else {
            panic!("expected the original response");
        };
        let json = serde_json::to_value(response).expect("response JSON");
        assert_eq!(json["id"], 41);
        assert_eq!(json["result"]["ok"], true);
    }

    #[test]
    fn reader_budget_refusal_consumes_nothing_and_preserves_the_first_event() {
        for expired in [false, true] {
            let probe = ProbeReader::new(b"data: retained\n\n".to_vec(), 2);
            let effects = Arc::clone(&probe.effects);
            let mut reader = SseReader::new(probe);
            let cx = if expired {
                expired_context()
            } else {
                cancelled_context()
            };
            let error = reader.read_event(&cx).expect_err("refuse exhausted budget");
            assert!(matches!(
                (&error, expired),
                (TransportError::Timeout, true) | (TransportError::Cancelled, false)
            ));
            assert_eq!(snapshot(&effects), ReadEffects::default());
            let event = reader
                .read_event(&Cx::for_testing())
                .expect("retry the same stream")
                .expect("retained event");
            assert_eq!(event.data, "retained");
        }
    }

    #[test]
    fn interrupted_read_retries_without_losing_the_event() {
        let probe = ProbeReader::new(b"data: retained\n\n".to_vec(), usize::MAX);
        probe.control.lock().expect("read control").interrupt_once = true;
        let effects = Arc::clone(&probe.effects);
        let mut reader = SseReader::new(probe);
        let event = reader
            .read_event(&Cx::for_testing())
            .expect("Interrupted is not EOF or corrupt framing")
            .expect("event");
        assert_eq!(event.data, "retained");
        assert_eq!(snapshot(&effects).calls, 2);
    }

    #[test]
    fn interrupted_read_then_cancellation_without_progress_is_retryable() {
        let cx = Cx::for_testing();
        let probe = ProbeReader::new(b"data: retained\n\n".to_vec(), usize::MAX);
        probe.control.lock().expect("read control").cancel_before_progress = Some(cx.clone());
        let effects = Arc::clone(&probe.effects);
        let mut reader = SseReader::new(probe);
        assert!(matches!(reader.read_event(&cx), Err(TransportError::Cancelled)));
        assert_eq!(snapshot(&effects), ReadEffects { calls: 1, bytes: 0 });
        let event = reader
            .read_event(&Cx::for_testing())
            .expect("no consumed bytes means no abandoned parser state")
            .expect("event");
        assert_eq!(event.data, "retained");
    }

    #[test]
    fn cancellation_inside_an_unterminated_line_stops_before_the_next_read() {
        for cancel in [false, true] {
            let cx = Cx::for_testing();
            let probe = ProbeReader::new(b"data: first\n\ndata: second\n\n".to_vec(), 3);
            probe.control.lock().expect("read control").cancel_after_progress =
                cancel.then(|| cx.clone());
            let effects = Arc::clone(&probe.effects);
            let mut reader = SseReader::new(probe);
            let result = reader.read_event(&cx);
            if !cancel {
                assert_eq!(result.expect("same reader without cancellation").expect("first").data, "first");
                let second = reader.read_event(&cx).expect("read").expect("second");
                assert_eq!(second.data, "second");
                continue;
            }
            assert!(matches!(result, Err(TransportError::Cancelled)));
            let stopped = snapshot(&effects);
            assert_eq!(stopped, ReadEffects { calls: 1, bytes: 3 });
            assert!(matches!(
                reader.read_event(&Cx::for_testing()),
                Err(TransportError::Closed)
            ));
            assert_eq!(snapshot(&effects), stopped, "must not parse the abandoned suffix");
        }
    }

    #[test]
    fn cr_lf_and_crlf_boundaries_remain_chunk_invariant() {
        let wire = b"id: 9\r\ndata: one\r\ndata: two\r\n\r\ndata: after\r\r";
        for chunk in [1, 2, 3, wire.len()] {
            let mut reader = SseReader::new(ProbeReader::new(wire.to_vec(), chunk));
            let first = reader.read_event(&Cx::for_testing()).expect("read").expect("first");
            assert_eq!(first.data, "one\ntwo", "chunk={chunk}");
            assert_eq!(first.id.as_deref(), Some("9"));
            let second = reader.read_event(&Cx::for_testing()).expect("read").expect("second");
            assert_eq!(second.data, "after", "chunk={chunk}");
            assert_eq!(second.id.as_deref(), Some("9"));
            assert!(reader.read_event(&Cx::for_testing()).expect("EOF").is_none());
        }
    }

    #[test]
    fn generic_client_receive_keeps_an_unconsumed_timeout_retryable() {
        let probe = ProbeReader::new(message_wire(), 3);
        let effects = Arc::clone(&probe.effects);
        let mut client = SseClientTransport::new(probe, Vec::<u8>::new());
        assert!(matches!(client.recv(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(snapshot(&effects), ReadEffects::default());
        assert_response(client.recv(&Cx::for_testing()).expect("same response after timeout"));
    }

    #[test]
    fn generic_client_endpoint_timeout_keeps_the_original_endpoint() {
        let endpoint = SseEvent::endpoint("/original").to_bytes().expect("endpoint");
        let probe = ProbeReader::new(endpoint, 3);
        let effects = Arc::clone(&probe.effects);
        let mut client = SseClientTransport::new(probe, Vec::<u8>::new());
        assert!(matches!(client.read_endpoint(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(snapshot(&effects), ReadEffects::default());
        assert_eq!(
            client.read_endpoint(&Cx::for_testing()).expect("retry").as_deref(),
            Some("/original")
        );
    }

    // This receive-only scenario must never attempt an outbound HTTP request.
    struct NoPost;

    impl LegacySsePostSink for NoPost {
        fn post(&mut self, _cx: &Cx, _post: LegacySseMessagePost) -> Result<(), TransportError> {
            panic!("receive and establish must not POST");
        }
    }

    #[test]
    fn exact_legacy_establish_and_receive_remain_retryable_before_progress() {
        let mut wire = SseEvent::endpoint("/original").to_bytes().expect("endpoint");
        wire.extend(message_wire());
        let probe = ProbeReader::new(wire, 3);
        let effects = Arc::clone(&probe.effects);
        let mut client = LegacySseClientTransport::new(probe, NoPost);
        assert!(matches!(client.establish(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(snapshot(&effects), ReadEffects::default());
        assert_eq!(client.advertised_endpoint(), None);
        assert_eq!(client.establish(&Cx::for_testing()).expect("retry"), "/original");
        let before_receive = snapshot(&effects);
        assert!(matches!(client.recv(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(snapshot(&effects), before_receive);
        assert_response(client.recv(&Cx::for_testing()).expect("preserved response"));
        assert_eq!(client.advertised_endpoint(), Some("/original"));
    }

    #[test]
    fn exact_legacy_client_cannot_resume_an_abandoned_message() {
        let mut wire = SseEvent::endpoint("/original").to_bytes().expect("endpoint");
        wire.extend(message_wire());
        let probe = ProbeReader::new(wire, 1);
        let effects = Arc::clone(&probe.effects);
        let control = Arc::clone(&probe.control);
        let mut client = LegacySseClientTransport::new(probe, NoPost);
        client.establish(&Cx::for_testing()).expect("establish");
        let before = snapshot(&effects);
        let cx = Cx::for_testing();
        control.lock().expect("read control").cancel_after_progress = Some(cx.clone());
        assert!(matches!(client.recv(&cx), Err(TransportError::Cancelled)));
        let stopped = snapshot(&effects);
        assert_eq!(stopped.calls, before.calls + 1);
        assert_eq!(stopped.bytes, before.bytes + 1);
        assert!(matches!(client.recv(&Cx::for_testing()), Err(TransportError::Closed)));
        assert_eq!(snapshot(&effects), stopped);
    }

    struct CountedRequests {
        requests: std::vec::IntoIter<JsonRpcRequest>,
        advances: Arc<AtomicUsize>,
    }

    impl Iterator for CountedRequests {
        type Item = JsonRpcRequest;

        fn next(&mut self) -> Option<Self::Item> {
            self.advances.fetch_add(1, Ordering::SeqCst);
            self.requests.next()
        }
    }

    fn requests(advances: &Arc<AtomicUsize>) -> CountedRequests {
        CountedRequests {
            requests: vec![JsonRpcRequest::new("tools/list", None, 73_i64)].into_iter(),
            advances: Arc::clone(advances),
        }
    }

    fn assert_request(message: JsonRpcMessage) {
        let JsonRpcMessage::Request(request) = message else {
            panic!("expected original request");
        };
        let json = serde_json::to_value(request).expect("request JSON");
        assert_eq!(json["id"], 73);
        assert_eq!(json["method"], "tools/list");
    }

    #[test]
    fn server_ingress_timeout_does_not_advance_or_discard_a_request() {
        let advances = Arc::new(AtomicUsize::new(0));
        let mut server = SseServerTransport::new(Vec::<u8>::new(), requests(&advances), "/mcp");
        assert!(matches!(server.recv(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(advances.load(Ordering::SeqCst), 0);
        assert_request(server.recv(&Cx::for_testing()).expect("retained request"));
        assert_eq!(advances.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn split_server_ingress_timeout_does_not_advance_or_discard_a_request() {
        let advances = Arc::new(AtomicUsize::new(0));
        let (mut recv, _send) =
            SseServerTransport::new(Vec::<u8>::new(), requests(&advances), "/mcp").into_split();
        assert!(matches!(recv.recv(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(advances.load(Ordering::SeqCst), 0);
        assert_request(recv.recv(&Cx::for_testing()).expect("retained request"));
        assert_eq!(advances.load(Ordering::SeqCst), 1);
    }

    fn response() -> JsonRpcMessage {
        JsonRpcMessage::Response(JsonRpcResponse::success(
            73.into(),
            serde_json::json!({"tools": []}),
        ))
    }

    #[test]
    fn refused_server_close_does_not_disable_ingress_or_egress() {
        let advances = Arc::new(AtomicUsize::new(0));
        let sink = CountingWriter::default();
        let mut server = SseServerTransport::new(sink.clone(), requests(&advances), "/mcp");
        assert!(matches!(server.close(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(sink.flushes(), 0);
        assert!(sink.wrote_nothing());
        assert_request(server.recv(&Cx::for_testing()).expect("close refusal preserved ingress"));
        server.send(&Cx::for_testing(), &response()).expect("close refusal preserved egress");
        assert_eq!(sink.flushes(), 2, "endpoint and response each committed once");
        server.close(&Cx::for_testing()).expect("live close");
        assert_eq!(sink.flushes(), 3);
        server.close(&expired_context()).expect("terminal close stays idempotent");
        assert_eq!(sink.flushes(), 3);
    }

    #[test]
    fn refused_split_send_close_does_not_disable_egress() {
        let advances = Arc::new(AtomicUsize::new(0));
        let sink = CountingWriter::default();
        let (_recv, mut send) =
            SseServerTransport::new(sink.clone(), requests(&advances), "/mcp").into_split();
        assert!(matches!(send.close(&expired_context()), Err(TransportError::Timeout)));
        assert_eq!(sink.flushes(), 0);
        assert!(sink.wrote_nothing());
        send.send(&Cx::for_testing(), &response()).expect("close refusal preserved egress");
        assert_eq!(sink.flushes(), 2);
        send.close(&Cx::for_testing()).expect("live close");
        send.close(&expired_context()).expect("idempotent close");
        assert_eq!(sink.flushes(), 3);
    }
}

/// The control: a live context commits and flushes exactly once.
#[test]
fn sse_close_under_a_live_context_commits_and_flushes() {
    let writer = CountingWriter::default();
    let mut sse = SseWriter::new(writer.clone());

    sse.close(&Cx::for_testing())
        .expect("a live caller context permits the terminal commit");

    assert_eq!(
        writer.flushes(),
        1,
        "the committed path flushes the write side exactly once"
    );
}

/// The variable: only the cancellation bit differs from the control.
#[test]
fn sse_close_under_a_cancelled_context_refuses_before_the_flush() {
    let writer = CountingWriter::default();
    let mut sse = SseWriter::new(writer.clone());

    let error = sse
        .close(&cancelled_context())
        .expect_err("a cancelled caller context must refuse the close");

    assert!(
        matches!(error, TransportError::Cancelled),
        "cancellation maps to Cancelled, not an I/O error: {error:?}"
    );
    assert_eq!(writer.flushes(), 0, "a refused close performs no flush");
    assert!(writer.wrote_nothing(), "a refused close writes nothing");
}

/// The row this module's older idiom could not produce. No cancellation bit is
/// set; only the deadline has expired. Under `cx.is_cancel_requested()` this
/// close would have returned `Ok` after flushing.
#[test]
fn sse_close_under_an_expired_deadline_refuses_as_timeout() {
    let writer = CountingWriter::default();
    let mut sse = SseWriter::new(writer.clone());

    let error = sse
        .close(&expired_context())
        .expect_err("an expired caller budget must refuse the close");

    assert!(
        matches!(error, TransportError::Timeout),
        "an exhausted deadline is reported as Timeout, distinctly from Cancelled: {error:?}"
    );
    assert_eq!(
        writer.flushes(),
        0,
        "an out-of-budget caller is not made to wait on the flush"
    );
}

/// A refusal is a retryable no-op, not a wedge. A budget-aware close that
/// stranded the writer on every cancelled caller would be worse than the
/// unbudgeted close it replaced.
#[test]
fn sse_close_refusal_leaves_the_writer_closable() {
    let writer = CountingWriter::default();
    let mut sse = SseWriter::new(writer.clone());

    sse.close(&cancelled_context())
        .expect_err("the cancelled attempt refuses");
    assert_eq!(writer.flushes(), 0);

    sse.close(&Cx::for_testing())
        .expect("a fresh budget still closes after a refusal");

    assert_eq!(
        writer.flushes(),
        1,
        "the retry performs the single flush the refusal skipped"
    );
}

/// Idempotency survives cancellation: a terminal close performs no I/O, so it
/// has no budget to spend and must not begin failing for callers whose original
/// request was cancelled. This is why the checkpoint sits after the
/// already-closed branch rather than at the top of the function.
#[test]
fn sse_close_stays_idempotent_under_a_cancelled_context() {
    let writer = CountingWriter::default();
    let mut sse = SseWriter::new(writer.clone());

    sse.close(&Cx::for_testing())
        .expect("the first close commits");
    assert_eq!(writer.flushes(), 1);

    sse.close(&cancelled_context())
        .expect("closing an already-closed writer stays Ok under cancellation");

    assert_eq!(
        writer.flushes(),
        1,
        "the idempotent close performs no second flush"
    );
}

// ===========================================================================
// SseClientTransport::close - flushes the POST request sink.
// ===========================================================================
//
// Same five-case shape as the SseWriter rows above and as both stdio closes.
// `SseClientTransport<R, W>` is generic over its sink, so the flush count is
// available here; it exposes no `is_closed()`, so the flush count carries every
// observable rather than sharing the work with a terminal-state check.

fn client_transport(sink: CountingWriter) -> SseClientTransport<std::io::Cursor<Vec<u8>>, CountingWriter> {
    SseClientTransport::new(std::io::Cursor::new(Vec::new()), sink)
}

/// Control.
#[test]
fn sse_client_close_under_a_live_context_commits_and_flushes() {
    let sink = CountingWriter::default();
    let mut transport = client_transport(sink.clone());

    transport
        .close(&Cx::for_testing())
        .expect("a live caller context permits the terminal commit");

    assert_eq!(sink.flushes(), 1, "the committed path flushes the sink once");
}

/// Limb one: cancellation.
#[test]
fn sse_client_close_under_a_cancelled_context_refuses_before_the_flush() {
    let sink = CountingWriter::default();
    let mut transport = client_transport(sink.clone());

    let error = transport
        .close(&cancelled_context())
        .expect_err("a cancelled caller context must refuse the close");

    assert!(
        matches!(error, TransportError::Cancelled),
        "cancellation maps to Cancelled: {error:?}"
    );
    assert_eq!(sink.flushes(), 0, "a refused close performs no flush");
}

/// Limb two: an expired deadline with no cancellation bit set.
#[test]
fn sse_client_close_under_an_expired_deadline_refuses_as_timeout() {
    let sink = CountingWriter::default();
    let mut transport = client_transport(sink.clone());

    let error = transport
        .close(&expired_context())
        .expect_err("an expired caller budget must refuse the close");

    assert!(
        matches!(error, TransportError::Timeout),
        "an exhausted deadline is reported distinctly from cancellation: {error:?}"
    );
    assert_eq!(
        sink.flushes(),
        0,
        "an out-of-budget caller is not made to wait on the flush"
    );
}

/// A refusal is retryable, not a wedge.
#[test]
fn sse_client_close_refusal_leaves_the_transport_closable() {
    let sink = CountingWriter::default();
    let mut transport = client_transport(sink.clone());

    transport
        .close(&cancelled_context())
        .expect_err("the cancelled attempt refuses");
    assert_eq!(sink.flushes(), 0);

    transport
        .close(&Cx::for_testing())
        .expect("a fresh budget still closes after a refusal");

    assert_eq!(
        sink.flushes(),
        1,
        "the retry performs the single flush the refusal skipped"
    );
}

/// Terminal close performs no I/O, so it stays Ok under cancellation.
#[test]
fn sse_client_close_stays_idempotent_under_a_cancelled_context() {
    let sink = CountingWriter::default();
    let mut transport = client_transport(sink.clone());

    transport
        .close(&Cx::for_testing())
        .expect("the first close commits");
    assert_eq!(sink.flushes(), 1);

    transport
        .close(&cancelled_context())
        .expect("closing an already-closed transport stays Ok under cancellation");

    assert_eq!(sink.flushes(), 1, "no second flush");
}

// Exercise the shipped implementations, not a replacement transport model.
mod egress_budget {
    use super::{cancelled_context, expired_context};
    use asupersync::Cx;
    use fastmcp_protocol::{JsonRpcMessage, JsonRpcRequest};
    use fastmcp_transport::sse::{
        LegacySseClientTransport, LegacySseMessagePost, LegacySsePostSink,
        SseClientTransport, SseEvent, SseReader, SseWriter,
    };
    use fastmcp_transport::{Transport, TransportError};
    use std::io::{Cursor, Error, ErrorKind, Write};
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Debug, Default, PartialEq, Eq)]
    struct Effects {
        bytes: Vec<u8>,
        writes: usize,
        flushes: usize,
    }

    // Real byte storage with explicit fault injection at the Write boundary.
    // All assertions below observe bytes/calls through this separate handle.
    struct ProbeWriter {
        effects: Arc<Mutex<Effects>>,
        chunk: usize,
        interrupt_write_once: bool,
        interrupt_flush_once: bool,
        cancel_before_progress: Option<Cx>,
        cancel_after_progress: Option<Cx>,
        cancel_in_flush: Option<Cx>,
        zero_write: bool,
    }

    impl ProbeWriter {
        fn new() -> (Self, Arc<Mutex<Effects>>) {
            let effects = Arc::new(Mutex::new(Effects::default()));
            (
                Self {
                    effects: Arc::clone(&effects),
                    chunk: usize::MAX,
                    interrupt_write_once: false,
                    interrupt_flush_once: false,
                    cancel_before_progress: None,
                    cancel_after_progress: None,
                    cancel_in_flush: None,
                    zero_write: false,
                },
                effects,
            )
        }
    }

    impl Write for ProbeWriter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.effects.lock().expect("probe lock").writes += 1;
            if let Some(cx) = self.cancel_before_progress.take() {
                cx.set_cancel_requested(true);
                return Err(Error::from(ErrorKind::Interrupted));
            }
            if std::mem::take(&mut self.interrupt_write_once) {
                return Err(Error::from(ErrorKind::Interrupted));
            }
            if self.zero_write {
                return Ok(0);
            }
            let written = bytes.len().min(self.chunk);
            self.effects
                .lock()
                .expect("probe lock")
                .bytes
                .extend_from_slice(&bytes[..written]);
            if let Some(cx) = self.cancel_after_progress.take() {
                cx.set_cancel_requested(true);
            }
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.effects.lock().expect("probe lock").flushes += 1;
            if std::mem::take(&mut self.interrupt_flush_once) {
                return Err(Error::from(ErrorKind::Interrupted));
            }
            if let Some(cx) = self.cancel_in_flush.take() {
                cx.set_cancel_requested(true);
            }
            Ok(())
        }
    }

    fn snapshot(effects: &Arc<Mutex<Effects>>) -> Effects {
        effects.lock().expect("probe lock").clone()
    }

    fn request() -> JsonRpcMessage {
        JsonRpcMessage::Request(JsonRpcRequest::new("tools/list", None, 41_i64))
    }

    #[derive(Clone, Copy, Debug)]
    enum Operation {
        Event,
        Endpoint,
        Message,
        Comment,
        KeepAlive,
    }

    fn invoke(
        operation: Operation,
        writer: &mut SseWriter<ProbeWriter>,
        cx: &Cx,
    ) -> Result<(), TransportError> {
        match operation {
            Operation::Event => writer.write_event(cx, &SseEvent::message("payload")),
            Operation::Endpoint => writer.write_endpoint(cx, "/messages"),
            Operation::Message => writer.write_message(cx, &request()),
            Operation::Comment => writer.write_comment(cx, "alive"),
            Operation::KeepAlive => writer.keep_alive(cx),
        }
    }

    fn expected_wire(operation: Operation) -> Vec<u8> {
        match operation {
            Operation::Event => SseEvent::message("payload").to_bytes().expect("event"),
            Operation::Endpoint => SseEvent::endpoint("/messages").to_bytes().expect("endpoint"),
            Operation::Message => SseEvent::message(
                r#"{"jsonrpc":"2.0","id":41,"method":"tools/list"}"#,
            )
            .with_id("1")
            .to_bytes()
            .expect("message"),
            Operation::Comment => b": alive\n".to_vec(),
            Operation::KeepAlive => b": keep-alive\n".to_vec(),
        }
    }

    #[test]
    fn every_writer_operation_refuses_expired_and_cancelled_budgets_before_io() {
        for operation in [
            Operation::Event,
            Operation::Endpoint,
            Operation::Message,
            Operation::Comment,
            Operation::KeepAlive,
        ] {
            for expired in [false, true] {
                let (probe, effects) = ProbeWriter::new();
                let mut writer = SseWriter::new(probe);
                let cx = if expired {
                    expired_context()
                } else {
                    cancelled_context()
                };
                let error = invoke(operation, &mut writer, &cx).expect_err("budget refusal");
                assert!(
                    matches!(
                        (&error, expired),
                        (TransportError::Timeout, true) | (TransportError::Cancelled, false)
                    ),
                    "{operation:?}: {error:?}"
                );
                assert_eq!(snapshot(&effects), Effects::default(), "{operation:?}");
                invoke(operation, &mut writer, &Cx::for_testing())
                    .expect("retry without lost state");
                let committed = snapshot(&effects);
                assert_eq!(committed.flushes, 1, "{operation:?}");
                // Compare decoded JSON for messages so object-member order is
                // not an accidental requirement of this transport test.
                if matches!(operation, Operation::Message) {
                    let mut reader = SseReader::new(Cursor::new(committed.bytes));
                    let event = reader
                        .read_event(&Cx::for_testing())
                        .expect("read")
                        .expect("event");
                    assert_eq!(event.id.as_deref(), Some("1"));
                    let json: serde_json::Value = serde_json::from_str(&event.data).expect("JSON");
                    assert_eq!(json["id"], 41);
                    assert_eq!(json["method"], "tools/list");
                } else {
                    assert_eq!(committed.bytes, expected_wire(operation), "{operation:?}");
                }
            }
        }
    }

    #[test]
    fn short_and_interrupted_writes_preserve_the_exact_frame() {
        let (mut probe, effects) = ProbeWriter::new();
        probe.chunk = 3;
        probe.interrupt_write_once = true;
        probe.interrupt_flush_once = true;
        let mut writer = SseWriter::new(probe);
        writer
            .write_event(&Cx::for_testing(), &SseEvent::message("payload"))
            .expect("short writes and interrupted operations are retried");
        let committed = snapshot(&effects);
        assert_eq!(committed.bytes, expected_wire(Operation::Event));
        assert_eq!(committed.writes, committed.bytes.len().div_ceil(3) + 1);
        assert_eq!(committed.flushes, 2);
    }

    #[test]
    fn cancellation_between_short_writes_terminalizes_the_event_stream() {
        for cancel in [false, true] {
            let cx = Cx::for_testing();
            let (mut probe, effects) = ProbeWriter::new();
            probe.chunk = 3;
            probe.cancel_after_progress = cancel.then(|| cx.clone());
            let mut writer = SseWriter::new(probe);
            let result = writer.write_event(&cx, &SseEvent::message("payload"));
            if !cancel {
                result.expect("same short writer without cancellation commits");
                assert_eq!(snapshot(&effects).bytes, expected_wire(Operation::Event));
                assert_eq!(snapshot(&effects).flushes, 1);
                continue;
            }
            assert!(matches!(result, Err(TransportError::Cancelled)));
            let stopped = snapshot(&effects);
            assert_eq!(stopped.bytes, b"eve");
            assert_eq!(stopped.writes, 1);
            assert_eq!(stopped.flushes, 0);
            assert!(matches!(
                writer.write_endpoint(&Cx::for_testing(), "/other"),
                Err(TransportError::Closed)
            ));
            writer
                .close(&Cx::for_testing())
                .expect("terminal close is a no-op");
            assert_eq!(snapshot(&effects), stopped);
        }
    }

    #[test]
    fn interrupted_write_without_progress_can_be_retried_without_skipping_event_id() {
        let cx = Cx::for_testing();
        let (mut probe, effects) = ProbeWriter::new();
        probe.cancel_before_progress = Some(cx.clone());
        let mut writer = SseWriter::new(probe);
        assert!(matches!(writer.write_message(&cx, &request()), Err(TransportError::Cancelled)));
        let refused = snapshot(&effects);
        assert!(refused.bytes.is_empty());
        assert_eq!(refused.writes, 1);
        assert_eq!(refused.flushes, 0);
        writer.write_message(&Cx::for_testing(), &request()).expect("safe retry");
        let mut reader = SseReader::new(Cursor::new(snapshot(&effects).bytes));
        let event = reader.read_event(&Cx::for_testing()).expect("read").expect("event");
        assert_eq!(event.id.as_deref(), Some("1"));
        let json: serde_json::Value = serde_json::from_str(&event.data).expect("JSON");
        assert_eq!(json["id"], 41);
    }

    #[test]
    fn cancellation_before_flush_does_not_enter_flush_or_allow_reuse() {
        let cx = Cx::for_testing();
        let (mut probe, effects) = ProbeWriter::new();
        probe.cancel_after_progress = Some(cx.clone());
        let mut writer = SseWriter::new(probe);
        assert!(matches!(writer.keep_alive(&cx), Err(TransportError::Cancelled)));
        let stopped = snapshot(&effects);
        assert_eq!(stopped.bytes, b": keep-alive\n");
        assert_eq!(stopped.flushes, 0);
        assert!(matches!(writer.keep_alive(&Cx::for_testing()), Err(TransportError::Closed)));
        assert_eq!(snapshot(&effects), stopped);
    }

    #[test]
    fn successful_flush_is_not_retroactively_changed_into_cancellation() {
        let cx = Cx::for_testing();
        let (mut probe, effects) = ProbeWriter::new();
        probe.cancel_in_flush = Some(cx.clone());
        let mut writer = SseWriter::new(probe);
        writer.keep_alive(&cx).expect("the frame was already committed");
        assert!(cx.is_cancel_requested());
        assert_eq!(snapshot(&effects).bytes, b": keep-alive\n");
        assert_eq!(snapshot(&effects).flushes, 1);
        writer.keep_alive(&Cx::for_testing()).expect("successful commit did not poison the stream");
        assert_eq!(snapshot(&effects).bytes, b": keep-alive\n: keep-alive\n");
    }

    #[test]
    fn zero_length_write_terminalizes_the_sink() {
        let (mut probe, effects) = ProbeWriter::new();
        probe.zero_write = true;
        let mut writer = SseWriter::new(probe);
        let error = writer.keep_alive(&Cx::for_testing()).expect_err("write zero");
        assert!(matches!(
            error,
            TransportError::Io(ref error) if error.kind() == ErrorKind::WriteZero
        ));
        assert!(matches!(writer.keep_alive(&Cx::for_testing()), Err(TransportError::Closed)));
        assert_eq!(snapshot(&effects).writes, 1);
        assert_eq!(snapshot(&effects).flushes, 0);
    }

    #[test]
    fn client_post_body_refuses_expired_budget_without_consuming_the_sink() {
        let (probe, effects) = ProbeWriter::new();
        let mut client = SseClientTransport::new(Cursor::new(Vec::<u8>::new()), probe);
        assert!(matches!(
            client.send(&expired_context(), &request()),
            Err(TransportError::Timeout)
        ));
        assert_eq!(snapshot(&effects), Effects::default());
        client.send(&Cx::for_testing(), &request()).expect("live retry");
        let committed = snapshot(&effects);
        assert_eq!(committed.flushes, 1);
        assert!(committed.bytes.ends_with(b"\n"));
        let json: serde_json::Value =
            serde_json::from_slice(&committed.bytes).expect("NDJSON request");
        assert_eq!(json["id"], 41);
        assert_eq!(json["method"], "tools/list");
    }

    #[test]
    fn cancellation_between_short_post_body_writes_is_terminal() {
        let cx = Cx::for_testing();
        let (mut probe, effects) = ProbeWriter::new();
        probe.chunk = 3;
        probe.cancel_after_progress = Some(cx.clone());
        let mut client = SseClientTransport::new(Cursor::new(Vec::<u8>::new()), probe);
        assert!(matches!(client.send(&cx, &request()), Err(TransportError::Cancelled)));
        let stopped = snapshot(&effects);
        assert_eq!(stopped.bytes.len(), 3);
        assert_eq!(stopped.writes, 1);
        assert_eq!(stopped.flushes, 0);
        assert!(matches!(client.send(&Cx::for_testing(), &request()), Err(TransportError::Closed)));
        assert_eq!(snapshot(&effects), stopped);
    }

    #[derive(Clone, Default)]
    struct PostLog(Arc<Mutex<Vec<RecordedPost>>>);

    type RecordedPost = (String, Vec<u8>);

    impl LegacySsePostSink for PostLog {
        fn post(&mut self, _cx: &Cx, post: LegacySseMessagePost) -> Result<(), TransportError> {
            self.0
                .lock()
                .expect("post log")
                .push((post.endpoint().to_owned(), post.body().to_vec()));
            Ok(())
        }
    }

    #[test]
    fn exact_legacy_adapter_refuses_expired_budget_before_calling_the_post_sink() {
        let posts = PostLog::default();
        let endpoint = SseEvent::endpoint("/messages?owner=one").to_bytes().expect("endpoint");
        let mut client = LegacySseClientTransport::new(Cursor::new(endpoint), posts.clone());
        assert_eq!(client.establish(&Cx::for_testing()).expect("establish"), "/messages?owner=one");
        assert!(matches!(
            client.send(&expired_context(), &request()),
            Err(TransportError::Timeout)
        ));
        assert!(posts.0.lock().expect("post log").is_empty());
        client.send(&Cx::for_testing(), &request()).expect("live retry");
        let posts = posts.0.lock().expect("post log");
        assert_eq!(posts.len(), 1);
        assert_eq!(posts[0].0, "/messages?owner=one");
        let json: serde_json::Value = serde_json::from_slice(&posts[0].1).expect("request body");
        assert_eq!(json["id"], 41);
    }
}
