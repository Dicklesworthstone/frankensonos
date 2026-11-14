//! An essential worker must not leave a live but unusable daemon behind.
//!
//! Listener and background-worker results stay in owned join handles.
//! On unexpected completion, stop the daemon through its usual drain and
//! return an error so the service manager can restart it. A failed worker
//! never resumes an interrupted operation inside the same process.

use anyhow::Context as _;
use fsonos_api::Surface;
use fsonos_api::surface::schedules::{SHUTDOWN_WAIT, Scheduler};
use std::any::Any;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) type Serving = Option<JoinHandle<anyhow::Result<()>>>;

/// Join only a finished worker: observing it must never hold up shutdown.
fn finished<T>(worker: &mut Option<JoinHandle<T>>) -> Option<thread::Result<T>> {
    if worker.as_ref().is_some_and(JoinHandle::is_finished) {
        worker.take().map(JoinHandle::join)
    } else {
        None
    }
}

fn panic_error(payload: &(dyn Any + Send)) -> anyhow::Error {
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or("non-string panic payload");
    anyhow::anyhow!("worker panicked: {message}")
}

fn unexpected(label: &str, result: thread::Result<anyhow::Result<()>>) -> anyhow::Error {
    let context = format!("{label} stopped unexpectedly");
    match result {
        Ok(Ok(())) => anyhow::anyhow!(context),
        Ok(Err(error)) => error.context(context),
        Err(payload) => panic_error(&*payload).context(context),
    }
}

pub(super) fn listener_failure(worker: &mut Serving, label: &str) -> Option<anyhow::Error> {
    finished(worker).map(|result| unexpected(label, result))
}

/// Retain the workers that keep playback and library state current. These
/// cannot be restarted in place: a panic may have interrupted speaker I/O.
/// Let the daemon's ordinary shutdown and service-manager restart recover.
pub(super) struct BackgroundWorkers {
    stop: Arc<AtomicBool>,
    threads: Vec<(String, Option<JoinHandle<()>>)>,
}

impl BackgroundWorkers {
    pub(super) fn new(stop: Arc<AtomicBool>) -> Self {
        Self {
            stop,
            threads: Vec::new(),
        }
    }

    pub(super) fn push(&mut self, thread: JoinHandle<()>) {
        let name = thread
            .thread()
            .name()
            .unwrap_or("background worker")
            .to_owned();
        self.threads.push((name, Some(thread)));
    }

    pub(super) fn failure(&mut self) -> Option<anyhow::Error> {
        self.threads.iter_mut().find_map(|(name, thread)| {
            finished(thread).map(|result| unexpected(name, result.map(Ok)))
        })
    }

    pub(super) fn shutdown(self) -> anyhow::Result<()> {
        self.shutdown_with(Duration::from_secs(5))
    }

    fn shutdown_with(mut self, wait: Duration) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        let deadline = Instant::now() + wait;
        let mut failure = None;
        for (name, worker) in &mut self.threads {
            while worker.as_ref().is_some_and(|worker| !worker.is_finished())
                && Instant::now() < deadline
            {
                thread::sleep(Duration::from_millis(20));
            }
            let error = if let Some(Err(payload)) = finished(worker) {
                Some(panic_error(&*payload).context(format!("{name} shutdown failed")))
            } else if worker.is_some() {
                Some(anyhow::anyhow!("{name} did not stop within {wait:?}"))
            } else {
                None
            };
            if let Some(error) = error {
                tracing::warn!("{error:#}");
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for BackgroundWorkers {
    fn drop(&mut self) {
        // A later listener/startup failure must stop already-started workers.
        self.stop.store(true, Ordering::Release);
    }
}

/// A signal is an ordinary shutdown. A worker exit before it is a failure,
/// even when its serving loop returned `Ok`: an essential port or the
/// scheduler is no longer available. Set the shared stop flag before the
/// caller drains work so every background loop participates in shutdown.
pub(super) fn wait(
    stop: &AtomicBool,
    mut failure: impl FnMut() -> Option<anyhow::Error>,
) -> anyhow::Result<()> {
    while !stop.load(Ordering::Acquire) {
        if let Some(error) = failure() {
            // A signal arriving during the check still wins over a worker
            // ending in response to that same signal.
            if !stop.swap(true, Ordering::AcqRel) {
                return Err(error);
            }
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

/// Retain the scheduler itself: a panic skips `Scheduler::start`'s normal
/// shutdown, but its outstanding fades still need to put the volumes back.
pub(super) struct SchedulerWorker {
    scheduler: Arc<Scheduler>,
    thread: Option<JoinHandle<()>>,
    panicked: bool,
}

impl SchedulerWorker {
    pub(super) fn start(surface: &Arc<Surface>, stop: Arc<AtomicBool>) -> anyhow::Result<Self> {
        let scheduler = Arc::new(Scheduler::new(surface));
        let thread = scheduler.start(stop).context("start the scheduler")?;
        Ok(Self {
            scheduler,
            thread: Some(thread),
            panicked: false,
        })
    }

    pub(super) fn failure(&mut self) -> Option<anyhow::Error> {
        let result = finished(&mut self.thread)?;
        self.panicked = result.is_err();
        Some(unexpected("scheduler", result.map(Ok)))
    }

    pub(super) fn shutdown(mut self) -> anyhow::Result<()> {
        let failure = self
            .thread
            .take()
            .and_then(|thread| thread.join().err())
            .map(|payload| panic_error(&*payload).context("scheduler shutdown failed"));
        if self.panicked || failure.is_some() {
            // Only cleanup is retried, never a tick or a claimed schedule.
            // A second panic must not prevent the announcement drain and
            // the rest of the daemon's cleanup from running.
            let cleanup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                self.scheduler.shut_down(SHUTDOWN_WAIT);
            }));
            if let Err(payload) = cleanup {
                let error = panic_error(&*payload).context("scheduler cleanup failed");
                return Err(match failure {
                    Some(first) => first.context(format!("{error:#}")),
                    None => error,
                });
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

#[cfg(test)]
mod tests;
