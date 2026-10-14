//! bd-odvyq O2/O3/O4: a caller-owned pool that REJECTS submission under a
//! current-thread driver, beside the near-identical pool that accepts.
//!
//! Every row is one fixture, `admission_row`. The rows differ only in whether
//! the pool is shut down before the runtime starts (O2 rejects, O3 accepts) and
//! in which guard the handler's sampling wait would use: `block_on` (the bridge
//! diagnosis the lane marker suppresses) or `lane.wait_for` (the driver refusal
//! a worker scope bypasses). Each row runs under `run_bounded`, an OS-thread
//! watchdog that a parked runtime driver cannot starve (O4).

use std::fmt;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::thread::{self, JoinHandle, ThreadId};
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::runtime::BlockingPool;
use fastmcp_core::{
    McpContext, McpErrorCode, McpOutcome, McpResult, Outcome, SamplingRequest, SamplingResponse,
};
use fastmcp_protocol::{Content, Tool};
use serde_json::{Value, json};

use super::super::{BlockingHandlerLane, BlockingTool};
use super::{runtime, sampling_peer};
use crate::handler::ToolHandler;

/// The one error a rejected caller-owned pool admission may produce.
const REJECTED_ADMISSION: &str = "blocking handler admission to caller blocking pool failed";

/// The lane's other refusals. A caller must be able to tell rejection apart
/// from each of them by message, because they all share `InternalError`.
const OTHER_LANE_REFUSALS: [&str; 4] = [
    "blocking handlers require an installed caller-owned blocking pool",
    "blocking handler lane is closed",
    "blocking handler panicked; payload redacted",
    "blocking handler admission to caller runtime failed",
];

/// Generous against load: an admitted or refused call completes in milliseconds.
const ROW_BOUND: Duration = Duration::from_secs(30);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Admission {
    Rejected,
    Accepted,
}

#[derive(Clone, Copy)]
enum GuardRow {
    BlockOn,
    WaitFor,
}

/// A bounded body that did not finish. Its `Display` is the failure message
/// every bounded test reports, and the body's thread stays joinable.
struct Expired<T> {
    label: &'static str,
    bound: Duration,
    body: JoinHandle<T>,
}

impl<T> fmt::Display for Expired<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "external bound '{}' expired after {:?}: the body did not complete",
            self.label, self.bound
        )
    }
}

/// Runs `body` on its own OS thread and waits for it on THIS thread's timer.
///
/// The bound is not a runtime timer: a body whose current-thread driver is
/// parked cannot delay it. A body panic is resumed here, so a failed assertion
/// inside the body reports as itself rather than as an expired bound.
fn run_bounded<T: Send + 'static>(
    label: &'static str,
    bound: Duration,
    body: impl FnOnce() -> T + Send + 'static,
) -> Result<T, Expired<T>> {
    let (done, finished) = mpsc::sync_channel::<()>(1);
    let handle = thread::Builder::new()
        .name(format!("bounded-{label}"))
        .spawn(move || {
            let value = body();
            let _ = done.send(());
            value
        })
        .expect("spawn the bounded body thread");
    match finished.recv_timeout(bound) {
        Ok(()) | Err(RecvTimeoutError::Disconnected) => match handle.join() {
            Ok(value) => Ok(value),
            Err(panic) => std::panic::resume_unwind(panic),
        },
        Err(RecvTimeoutError::Timeout) => Err(Expired {
            label,
            bound,
            body: handle,
        }),
    }
}

fn bounded_row(label: &'static str, admission: Admission, row: GuardRow) {
    if let Err(expired) = run_bounded(label, ROW_BOUND, move || admission_row(admission, row)) {
        panic!("{expired}");
    }
}

/// A synchronous tool whose only work is one sampling round trip.
struct RowTool {
    calls: Arc<AtomicUsize>,
    ran_on: Arc<Mutex<Option<ThreadId>>>,
    poller: ThreadId,
    lane: Option<BlockingHandlerLane>,
}

impl ToolHandler for RowTool {
    fn definition(&self) -> Tool {
        Tool {
            name: "blocking_rejection_row".into(),
            description: None,
            input_schema: json!({"type":"object"}),
            output_schema: None,
            icon: None,
            version: None,
            tags: vec![],
            annotations: None,
        }
    }

    fn call(&self, ctx: &McpContext, _arguments: Value) -> McpResult<Vec<Content>> {
        // Counted before any assertion, so an inline run on the driver is
        // still visible after its assertion panic is redacted.
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.ran_on.lock().unwrap() = Some(thread::current().id());
        assert_ne!(
            thread::current().id(),
            self.poller,
            "handler must not run on the poller"
        );
        assert_eq!(ctx.request_id(), 7);
        let request = ctx.sample("complete from peer", 17);
        let response = match &self.lane {
            Some(lane) => lane.wait_for(request),
            None => fastmcp_core::block_on(request),
        }?;
        Ok(vec![Content::Text {
            text: response.text,
        }])
    }
}

/// Drives `call` to completion, answering the peer's sampling request if one
/// arrives. Returns the call's outcome and whether a request was ever seen.
async fn drive_exchange<C>(
    cx: &Cx,
    call: C,
    received: &mut asupersync::channel::oneshot::Receiver<SamplingRequest>,
    reply: asupersync::channel::oneshot::Sender<SamplingResponse>,
) -> (C::Output, bool)
where
    C: Future,
{
    let mut call = std::pin::pin!(call);
    let mut requested = std::pin::pin!(received.recv(cx));
    let mut reply = Some(reply);
    let result = std::future::poll_fn(|task| {
        if let Poll::Ready(result) = call.as_mut().poll(task) {
            return Poll::Ready(result);
        }
        if reply.is_some() {
            if let Poll::Ready(request) = requested.as_mut().poll(task) {
                let request = request.unwrap();
                assert_eq!(request.messages[0].text, "complete from peer");
                assert_eq!(request.max_tokens, 17);
                reply
                    .take()
                    .unwrap()
                    .send_blocking(SamplingResponse::new("peer completion", "test-model"))
                    .unwrap();
            }
        }
        Poll::Pending
    })
    .await;
    (result, reply.is_none())
}

fn admission_row(admission: Admission, row: GuardRow) {
    let rejected = admission == Admission::Rejected;
    let pool = BlockingPool::new(0, 1);
    if rejected {
        pool.shutdown();
    }
    // The ONE input variable between O2 and O3 is fixed before the runtime starts.
    assert_eq!(pool.is_shutdown(), rejected);
    let calls = Arc::new(AtomicUsize::new(0));
    let ran_on = Arc::new(Mutex::new(None));
    let poller = runtime(false).block_on(async {
        let cx = Cx::current()
            .unwrap()
            .with_blocking_pool_handle(Some(pool.handle()));
        let (sampler, mut received, reply) = sampling_peer(&cx);
        let context = McpContext::new(cx.clone(), 7).with_sampling(sampler.clone());
        let lane = BlockingHandlerLane::new(1).unwrap();
        let poller = thread::current().id();
        let tool = BlockingTool::new(
            RowTool {
                calls: Arc::clone(&calls),
                ran_on: Arc::clone(&ran_on),
                poller,
                lane: matches!(row, GuardRow::WaitFor).then(|| lane.clone()),
            },
            lane.clone(),
        )
        .unwrap();
        let (outcome, peer_asked) = drive_exchange(
            &cx,
            tool.call_async(&context, json!({})),
            &mut received,
            reply,
        )
        .await;
        let observed = RowObservation {
            handler_calls: calls.load(Ordering::SeqCst),
            sampler_calls: sampler.calls.load(Ordering::SeqCst),
            peer_asked,
        };
        if rejected {
            assert_rejected(outcome, &observed);
        } else {
            assert_accepted(outcome, &observed);
        }
        assert_eq!(lane.in_flight().unwrap(), 0);
        assert_driver_guards_survive(&context, &lane, &sampler.calls, usize::from(!rejected));
        poller
    });
    let worker = *ran_on.lock().unwrap();
    if let Some(worker) = worker {
        assert_ne!(
            worker, poller,
            "the admitted handler must run off the poller"
        );
    } else {
        assert!(rejected, "an accepting pool must have run the handler");
    }
    assert_submission_matches_admission(&pool, rejected);
    assert!(pool.shutdown_and_wait(Duration::from_secs(5)));
}

/// What a row's call left behind, read on the driver right after it returned.
struct RowObservation {
    handler_calls: usize,
    sampler_calls: usize,
    peer_asked: bool,
}

/// O2(a)-(c): an exact, nameable refusal, with no handler run and no reverse
/// request.
fn assert_rejected(outcome: McpOutcome<Vec<Content>>, observed: &RowObservation) {
    let error = match outcome {
        Outcome::Err(error) => error,
        other => panic!("a rejected pool admission must return an error, observed {other:?}"),
    };
    assert_eq!(error.code, McpErrorCode::InternalError);
    assert_eq!(error.message, REJECTED_ADMISSION);
    assert!(error.data.is_none());
    for other in OTHER_LANE_REFUSALS {
        assert_ne!(
            error.message, other,
            "rejection must be nameable apart from {other:?}"
        );
    }
    assert_eq!(
        observed.handler_calls, 0,
        "a rejected handler must never run"
    );
    assert!(
        !observed.peer_asked,
        "no sampling request may leave for a rejected handler"
    );
    assert_eq!(observed.sampler_calls, 0);
}

/// O3: the accepting pool runs the handler once and completes its exchange.
fn assert_accepted(outcome: McpOutcome<Vec<Content>>, observed: &RowObservation) {
    let content = match outcome {
        Outcome::Ok(content) => content,
        other => {
            panic!("an accepting pool must complete the sampling exchange, observed {other:?}")
        }
    };
    assert!(matches!(&content[..], [Content::Text { text }] if text == "peer completion"));
    assert_eq!(
        observed.handler_calls, 1,
        "an admitted handler runs exactly once"
    );
    assert!(
        observed.peer_asked,
        "the admitted handler's request must reach the peer"
    );
    assert_eq!(observed.sampler_calls, 1);
}

/// O2(e): afterwards, on the same driver, neither guard the defect disabled
/// has been disabled: no leaked lane marker, no leaked worker scope.
fn assert_driver_guards_survive(
    context: &McpContext,
    lane: &BlockingHandlerLane,
    sampler_calls: &AtomicUsize,
    expected_sampler_calls: usize,
) {
    let error = fastmcp_core::block_on(context.sample("must not send", 17)).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("Sampling cannot complete from here"),
        "{error}"
    );
    let polls = AtomicUsize::new(0);
    assert!(
        lane.wait_for(async {
            polls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .is_err()
    );
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    assert_eq!(sampler_calls.load(Ordering::SeqCst), expected_sampler_calls);
    assert!(context.ensure_live().is_ok());
}

/// Proves the pool's admission state directly, on the SAME pool, after the call,
/// so the probe cannot change what the call itself saw.
fn assert_submission_matches_admission(pool: &BlockingPool, rejected: bool) {
    let ran = Arc::new(AtomicBool::new(false));
    let marker = Arc::clone(&ran);
    let probe = pool
        .handle()
        .spawn(move || marker.store(true, Ordering::SeqCst));
    if rejected {
        // A shut pool answers at submission: already cancelled and done, with
        // the closure dropped unrun.
        assert!(
            probe.is_cancelled() && probe.is_done(),
            "a shut pool must refuse direct submission"
        );
        assert!(
            !ran.load(Ordering::SeqCst),
            "a refused submission must never run"
        );
    } else {
        assert!(
            probe.wait_timeout(Duration::from_secs(5)),
            "an accepting pool must run direct submission"
        );
        assert!(!probe.is_cancelled());
        assert!(ran.load(Ordering::SeqCst));
    }
}

#[test]
fn rejected_pool_refuses_block_on_sampling_handler_before_any_effect() {
    bounded_row("o2-block-on", Admission::Rejected, GuardRow::BlockOn);
}

#[test]
fn rejected_pool_refuses_wait_for_sampling_handler_before_any_effect() {
    bounded_row("o2-wait-for", Admission::Rejected, GuardRow::WaitFor);
}

#[test]
fn accepting_pool_runs_block_on_sampling_handler_off_the_poller() {
    bounded_row("o3-block-on", Admission::Accepted, GuardRow::BlockOn);
}

#[test]
fn accepting_pool_runs_wait_for_sampling_handler_off_the_poller() {
    bounded_row("o3-wait-for", Admission::Accepted, GuardRow::WaitFor);
}

/// O4's fires-control: the SAME helper around a body that parks its
/// current-thread driver inside a poll, exactly as the defect would.
#[test]
fn external_bound_fails_a_parked_driver_by_name() {
    let bound = Duration::from_millis(250);
    let (release, parked) = mpsc::channel::<()>();
    let started = Instant::now();
    let result = run_bounded("fires-control", bound, move || {
        runtime(false).block_on(async move {
            parked
                .recv()
                .expect("the control releases its parked driver");
        });
    });
    let Err(expired) = result else {
        panic!("a parked driver must not complete within its external bound");
    };
    assert!(started.elapsed() >= bound);
    assert_eq!(
        expired.to_string(),
        "external bound 'fires-control' expired after 250ms: the body did not complete",
    );
    release.send(()).unwrap();
    expired.body.join().expect("the released body completes");
}
