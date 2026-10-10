//! Request-bound waiting on an admitted blocking worker, not a private runtime.

use std::cell::RefCell;
use std::future::Future;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, Wake, Waker};
use std::time::Duration;

use asupersync::{Cx, channel::oneshot};
use fastmcp_core::{McpContext, McpError, McpResult};

use super::{BlockingHandlerLane, LaneInner};

// Periodic liveness checks also observe tightened budgets and closed request
// leases, which need not wake the operation. This is not a new deadline clock.
const LIVENESS_INTERVAL: Duration = Duration::from_millis(10);

struct ActiveWorker {
    lane: Arc<LaneInner>,
    context: McpContext,
    waiting: bool,
}

thread_local! {
    static ACTIVE_WORKER: RefCell<Option<ActiveWorker>> = const { RefCell::new(None) };
}

/// Only the adapter's verified pool closure can install this scope. A public
/// core blocking-lane declaration alone does not admit use of this bridge.
pub(super) struct WorkerScope {
    previous: Option<ActiveWorker>,
    // TLS guards must neither move to nor be shared with another thread.
    _thread_bound: PhantomData<Rc<()>>,
}

impl WorkerScope {
    pub(super) fn enter(lane: Arc<LaneInner>, context: McpContext) -> Self {
        Self {
            previous: ACTIVE_WORKER.with(|slot| {
                slot.replace(Some(ActiveWorker { lane, context, waiting: false }))
            }),
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for WorkerScope {
    fn drop(&mut self) {
        ACTIVE_WORKER.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

struct WaitEntry {
    context: McpContext,
    _thread_bound: PhantomData<Rc<()>>,
}

impl WaitEntry {
    fn enter(lane: &BlockingHandlerLane) -> McpResult<Self> {
        lane.verify()?;
        ACTIVE_WORKER.with(|slot| {
            let mut slot = slot.borrow_mut();
            let worker = slot.as_mut().ok_or_else(|| McpError::invalid_request(
                "blocking wait requires an admitted blocking handler worker",
            ))?;
            if !Arc::ptr_eq(&worker.lane, &lane.inner) {
                return Err(McpError::invalid_request(
                    "blocking wait belongs to a different handler lane",
                ));
            }
            if worker.waiting {
                return Err(McpError::invalid_request("nested blocking handler waits are not supported"));
            }
            worker.context.ensure_live().map_err(|_| McpError::request_cancelled())?;
            let context = worker.context.clone();
            worker.waiting = true;
            Ok(Self { context, _thread_bound: PhantomData })
        })
    }
}

impl Drop for WaitEntry {
    fn drop(&mut self) {
        ACTIVE_WORKER.with(|slot| {
            if let Some(worker) = slot.borrow_mut().as_mut() {
                worker.waiting = false;
            }
        });
    }
}

#[derive(Default)]
struct WakeSignal {
    notified: Mutex<bool>,
    changed: Condvar,
}

impl WakeSignal {
    fn notify(&self) {
        *self.notified.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_one();
    }

    fn wait(&self) {
        let mut notified = self.notified.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*notified {
            let (guard, _) = self.changed.wait_timeout(notified, LIVENESS_INTERVAL)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            notified = guard;
        }
        *notified = false;
    }
}

impl Wake for WakeSignal {
    fn wake(self: Arc<Self>) { self.notify(); }
    fn wake_by_ref(self: &Arc<Self>) { self.notify(); }
}

impl BlockingHandlerLane {
    /// Waits for one asynchronous operation inside a handler admitted to this
    /// lane. For example, a synchronous tool can call
    /// `self.lane.wait_for(ctx.sample("Summarize this result", 128))`.
    ///
    /// This uses the current worker's retained request context. It creates no
    /// runtime, reactor, timer driver, thread, or fresh request budget. The
    /// caller must keep driving its runtime, including any peer response pump.
    /// Synchronous tools, resources, prompts, and completion handlers registered
    /// through this lane's adapters can use the same operation.
    ///
    /// Request/runtime cancellation, deadline expiry, and lease closure are
    /// observed before and after polling, even when the operation never wakes.
    /// Cancellation drops the operation on this worker; the lane remains
    /// charged until the enclosing handler and result custody actually end.
    /// The future's poll and drop implementations must not block. Neither this
    /// bridge nor the adapter can preempt a blocking syscall or destructor.
    ///
    /// Do not await work which requires another slot on an exhausted blocking
    /// pool. Only independent caller-driven operations can make progress while
    /// this worker waits. Closing lane admission does not cancel existing work.
    ///
    /// # Errors
    ///
    /// Rejects use outside this lane's admitted worker, from a different lane,
    /// or recursively inside another wait, without polling the supplied future.
    /// Returns request cancellation if its retained context is no longer live,
    /// and otherwise preserves the operation's exact result or error.
    pub fn wait_for<T>(&self, operation: impl Future<Output = McpResult<T>>) -> McpResult<T> {
        let entry = WaitEntry::enter(self)?;
        let ctx = &entry.context;
        let _current = Cx::set_current(Some(ctx.cx().clone()));
        // Bind futures after the scope and ambient context so their cleanup
        // runs before either is released, including when polling panics.
        let mut operation = std::pin::pin!(operation);
        let mut request_cancelled = std::pin::pin!(ctx.request_cancelled());
        let (_sender, mut receiver) = oneshot::channel::<()>();
        let mut runtime_cancelled = std::pin::pin!(receiver.recv(ctx.cx()));
        let signal = Arc::new(WakeSignal::default());
        let waker = Waker::from(Arc::clone(&signal));
        let mut task = Context::from_waker(&waker);

        loop {
            self.verify()?;
            ctx.ensure_live().map_err(|_| McpError::request_cancelled())?;
            if request_cancelled.as_mut().poll(&mut task).is_ready()
                || runtime_cancelled.as_mut().poll(&mut task).is_ready()
            {
                return Err(McpError::request_cancelled());
            }
            let result = operation.as_mut().poll(&mut task);
            ctx.ensure_live().map_err(|_| McpError::request_cancelled())?;
            if let Poll::Ready(result) = result {
                return result;
            }
            // A private condition variable avoids consuming another bridge's
            // thread::park token. Its retained bit also covers wake-before-wait.
            signal.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{pending, poll_fn};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use asupersync::time::Sleep;

    fn runtime() -> asupersync::runtime::Runtime {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .blocking_threads(1, 1)
            .build().unwrap()
    }

    fn cleanup_context(cx: &Cx) -> McpContext {
        McpContext::new(cx.clone(), 99).with_operation_deadline(Some(
            cx.now().saturating_add_nanos(5_000_000_000),
        ))
    }

    #[test]
    fn wait_uses_the_admitted_context_and_the_callers_timer_driver() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let poller = std::thread::current().id();
            let result = lane.execute(&ctx, &cx, move |worker_ctx| {
                assert_ne!(std::thread::current().id(), poller);
                let expected_task = worker_ctx.task_id();
                let borrowed = String::from("borrowed result");
                admitted.wait_for(async {
                    assert_eq!(Cx::current().unwrap().task_id(), expected_task);
                    Sleep::new(worker_ctx.cx().now().saturating_add_nanos(1_000_000)).await;
                    assert_eq!(Cx::current().unwrap().task_id(), expected_task);
                    assert_eq!(worker_ctx.request_id(), 7);
                    Ok(borrowed.len())
                })
            }).await.unwrap();
            assert_eq!(result, "borrowed result".len());
            assert_eq!(lane.in_flight().unwrap(), 0);
            assert!(ctx.ensure_live().is_ok());
        });
    }

    #[test]
    fn driver_wrong_lane_and_nested_waits_refuse_without_polling() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let polls = Arc::new(AtomicUsize::new(0));
            // Even manually declaring a core blocking lane grants no worker.
            let declared = fastmcp_core::runtime::enter_blocking_lane();
            assert!(lane.wait_for(async { polls.fetch_add(1, Ordering::SeqCst); Ok(1) }).is_err());
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            drop(declared);
            let admitted = lane.clone();
            let other = BlockingHandlerLane::new(1).unwrap();
            let observed = Arc::clone(&polls);
            let result = lane.execute(&ctx, &cx, move |_| {
                assert!(other.wait_for(async { observed.fetch_add(1, Ordering::SeqCst); Ok(2) }).is_err());
                admitted.wait_for(async {
                    assert!(admitted.wait_for(async { observed.fetch_add(1, Ordering::SeqCst); Ok(3) }).is_err());
                    Ok(41)
                })
            }).await.unwrap();
            assert_eq!(result, 41);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }

    struct DropObserved(Arc<AtomicUsize>);
    impl Drop for DropObserved {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    #[test]
    fn cancellation_drops_a_non_waking_operation_and_recovers_worker_capacity() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let (started, mut entered) = oneshot::channel::<()>();
            let mut call = Box::pin(lane.execute(&ctx, &cx, move |_| {
                let result = admitted.wait_for(async move {
                    let _drop = DropObserved(observed);
                    started.send_blocking(()).unwrap();
                    pending::<McpResult<()>>().await
                });
                assert_eq!(admitted.in_flight().unwrap(), 1, "the enclosing worker still owns its reservation");
                result
            }));
            poll_fn(|task| { assert!(call.as_mut().poll(task).is_pending()); Poll::Ready(()) }).await;
            entered.recv(&cx).await.unwrap();
            ctx.request_cancellation().cancel();
            assert!(call.await.is_err());
            let sibling = cleanup_context(&cx);
            lane.wait_idle(&sibling).await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(lane.in_flight().unwrap(), 0);
            assert_eq!(lane.execute(&sibling, &cx, |_| Ok(42)).await.unwrap(), 42);
            assert!(sibling.ensure_live().is_ok());
        });
    }

    #[test]
    fn abandoning_the_async_call_cancels_its_waiting_worker_not_a_sibling() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let (started, mut entered) = oneshot::channel::<()>();
            let mut call = Box::pin(lane.execute(&ctx, &cx, move |_| {
                admitted.wait_for(async move {
                    let _drop = DropObserved(observed);
                    started.send_blocking(()).unwrap();
                    pending::<McpResult<()>>().await
                })
            }));
            poll_fn(|task| { assert!(call.as_mut().poll(task).is_pending()); Poll::Ready(()) }).await;
            entered.recv(&cx).await.unwrap();
            drop(call);
            let sibling = cleanup_context(&cx);
            lane.wait_idle(&sibling).await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(lane.in_flight().unwrap(), 0);
            assert_eq!(lane.execute(&sibling, &cx, |_| Ok(43)).await.unwrap(), 43);
            assert!(ctx.ensure_live().is_ok(), "worker abort must not cancel the calling context");
            assert!(sibling.ensure_live().is_ok());
        });
    }

    #[test]
    fn deadline_stops_a_non_waking_wait_without_releasing_capacity_early() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7).with_operation_deadline(Some(
                cx.now().saturating_add_nanos(1_000_000_000),
            ));
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let result = lane.execute(&ctx, &cx, move |_| {
                let result = admitted.wait_for(async move {
                    let _drop = DropObserved(observed);
                    pending::<McpResult<()>>().await
                });
                assert_eq!(admitted.in_flight().unwrap(), 1);
                result
            }).await;
            assert!(result.is_err());
            lane.wait_idle(&cleanup_context(&cx)).await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(lane.in_flight().unwrap(), 0);
            assert!(cx.checkpoint().is_ok(), "request deadline must not cancel the caller");
        });
    }

    #[test]
    fn cancellation_during_poll_discards_the_ready_value_and_unpolled_work() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let drops = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&drops);
            let polls = Arc::new(AtomicUsize::new(0));
            let forbidden = Arc::clone(&polls);
            assert!(lane.execute(&ctx, &cx, move |worker_ctx| {
                assert!(admitted.wait_for(async {
                    worker_ctx.request_cancellation().cancel();
                    Ok(DropObserved(observed))
                }).is_err());
                assert!(admitted.wait_for(async { forbidden.fetch_add(1, Ordering::SeqCst); Ok(1) }).is_err());
                Ok(())
            }).await.is_err());
            lane.wait_idle(&cleanup_context(&cx)).await.unwrap();
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(polls.load(Ordering::SeqCst), 0);
        });
    }

    #[test]
    fn wait_scope_unwinds_without_leaking_to_reused_workers() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let error = lane.execute::<(), _>(&ctx, &cx, move |_| {
                admitted.wait_for(async { panic!("private-wait-panic-canary") })
            }).await.unwrap_err();
            assert!(!error.to_string().contains("private-wait-panic-canary"));
            let probe_lane = lane.clone();
            let mut probe = cx.spawn_blocking(move |_| {
                probe_lane.wait_for(async { panic!("unadmitted future must not be polled") })
                    .map(|(): ()| ())
            }).unwrap();
            assert!(probe.join(&cx).await.unwrap().is_err());
            let admitted = lane.clone();
            assert_eq!(lane.execute(&ctx, &cx, move |_| {
                admitted.wait_for(async { Ok(73) })
            }).await.unwrap(), 73);
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }

    #[test]
    fn closed_admission_allows_current_wait_and_preserves_operation_errors() {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let ctx = McpContext::new(cx.clone(), 7);
            let lane = BlockingHandlerLane::new(1).unwrap();
            let admitted = lane.clone();
            let value = lane.execute(&ctx, &cx, move |_| {
                admitted.close()?;
                let error = admitted.wait_for::<()>(async {
                    Err(McpError::invalid_params("preserved-operation-error"))
                }).unwrap_err();
                assert!(error.to_string().contains("preserved-operation-error"));
                admitted.wait_for(async { Ok(19) })
            }).await.unwrap();
            assert_eq!(value, 19);
            assert!(lane.execute(&ctx, &cx, |_| Ok(20)).await.is_err());
            assert_eq!(lane.in_flight().unwrap(), 0);
        });
    }
}
