//! Work on a thread of its own, awaited. A listener's handlers and the MCP
//! tools run as they are called, so a call that takes long (an announcement
//! that plays, a fade, a DJ start asking Spotify, the doctor probing the LAN)
//! would hold one of the listener's few worker threads for all that time;
//! enough of them at once and every other request waits. Through
//! [`off_thread`] the thread waits instead, and the workers stay free. At most
//! [`MAX_CALLS`] run at once: past that a call is refused as busy rather than
//! queued, so a caller looping on a slow request cannot exhaust the host.

use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll, Waker};

use crate::failure::{ErrorCode, Failure};

/// The most calls on threads of their own at once, across the daemon.
pub const MAX_CALLS: usize = 64;

static RUNNING: AtomicUsize = AtomicUsize::new(0);

/// What the thread came back with.
struct Slot<T> {
    value: Option<T>,
    waker: Option<Waker>,
    done: bool,
}

/// `work`'s answer, once its thread has it: `None` if the work panicked or
/// no thread could start.
pub struct OffThread<T>(Arc<Mutex<Slot<T>>>);

/// Run `work` on a thread of its own; await its answer.
///
/// # Errors
/// `NOT_READY` when [`MAX_CALLS`] are running already.
pub fn off_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<OffThread<T>, Failure> {
    bounded(&RUNNING, MAX_CALLS, work)
}

/// Gives a reserved place back when its thread ends, however it ends.
struct Place(&'static AtomicUsize);

impl Drop for Place {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn bounded<T: Send + 'static>(
    running: &'static AtomicUsize,
    max: usize,
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<OffThread<T>, Failure> {
    if running.fetch_add(1, Ordering::AcqRel) >= max {
        running.fetch_sub(1, Ordering::AcqRel);
        return Err(Failure::new(
            ErrorCode::NotReady,
            format!("the daemon is answering {max} calls that take long already"),
        )
        .with_hint("Retry in a moment."));
    }
    let place = Place(running);
    let slot = Arc::new(Mutex::new(Slot {
        value: None,
        waker: None,
        done: false,
    }));
    let feed = Arc::clone(&slot);
    let spawned = std::thread::Builder::new()
        .name("fsonos-call".into())
        .spawn(move || {
            let _place = place;
            let value = std::panic::catch_unwind(AssertUnwindSafe(work)).ok();
            let mut s = feed.lock().unwrap_or_else(PoisonError::into_inner);
            s.value = value;
            s.done = true;
            if let Some(waker) = s.waker.take() {
                waker.wake();
            }
        });
    if spawned.is_err() {
        // The closure (and its place) went with the failed spawn.
        slot.lock().unwrap_or_else(PoisonError::into_inner).done = true;
    }
    Ok(OffThread(slot))
}

impl<T> Future for OffThread<T> {
    type Output = Option<T>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<T>> {
        let mut s = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if s.done {
            return Poll::Ready(s.value.take());
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    /// Drive `f` to completion on this thread (a test has no runtime).
    fn block_on<F: Future>(f: F) -> F::Output {
        struct Wake(std::thread::Thread);
        impl std::task::Wake for Wake {
            fn wake(self: Arc<Self>) {
                self.0.unpark();
            }
        }
        let waker = Waker::from(Arc::new(Wake(std::thread::current())));
        let mut cx = Context::from_waker(&waker);
        let mut f = std::pin::pin!(f);
        loop {
            if let Poll::Ready(out) = f.as_mut().poll(&mut cx) {
                return out;
            }
            std::thread::park();
        }
    }

    #[test]
    fn the_work_happens_on_a_thread_of_its_own() {
        let (go, wait) = mpsc::channel::<()>();
        let waiting = off_thread(move || {
            wait.recv_timeout(Duration::from_secs(5))
                .expect("released by the caller, which did not wait for this");
            7
        })
        .unwrap();
        // Here while the work still waits for us: it runs elsewhere.
        go.send(()).unwrap();
        assert_eq!(block_on(waiting), Some(7));
    }

    #[test]
    fn a_panic_is_an_answer_too() {
        let failed = off_thread(|| -> u8 { panic!("the work broke") }).unwrap();
        assert_eq!(block_on(failed), None, "never left waiting");
    }

    #[test]
    fn past_the_limit_a_call_is_refused_busy_and_its_place_comes_back() {
        static RUNNING: AtomicUsize = AtomicUsize::new(0);
        let (release, held) = mpsc::channel::<()>();
        let held = Arc::new(Mutex::new(held));
        let waiting: Vec<_> = (0..3)
            .map(|_| {
                let held = Arc::clone(&held);
                bounded(&RUNNING, 3, move || {
                    let _ = held.lock().unwrap().recv();
                })
                .unwrap()
            })
            .collect();
        let busy = bounded(&RUNNING, 3, || ())
            .err()
            .expect("a fourth is refused");
        assert_eq!(busy.code, ErrorCode::NotReady);
        for _ in 0..3 {
            release.send(()).unwrap();
        }
        for w in waiting {
            assert_eq!(block_on(w), Some(()));
        }
        // Every place is back once the threads have ended.
        let deadline = Instant::now() + Duration::from_secs(5);
        while RUNNING.load(Ordering::Acquire) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(RUNNING.load(Ordering::Acquire), 0);
        assert_eq!(block_on(bounded(&RUNNING, 3, || 1).unwrap()), Some(1));
    }
}
