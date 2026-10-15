use super::*;
use fastmcp::ContentBlock;
use serde_json::json;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

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
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return value;
        }
        assert!(Instant::now() < deadline, "worker did not finish");
        std::thread::park_timeout(Duration::from_millis(10));
    }
}

fn answer() -> FinalCallToolResult {
    FinalCallToolResult {
        content: vec![ContentBlock::text("selected queue entry")],
        is_error: false,
        structured_content: Some(json!({"position": 3, "revision": "opaque:7"})),
    }
}

#[test]
fn a_blocked_control_yields_without_holding_the_request_thread() {
    let (release, held) = mpsc::channel();
    let (started, running) = mpsc::channel();
    let caller = std::thread::current().id();
    let mut waiting = Box::pin(perform(move || {
        started.send(std::thread::current().id()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        Ok(answer())
    }));
    let wake = waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&wake))
            .is_pending()
    );
    assert_ne!(
        running.recv_timeout(Duration::from_secs(5)).unwrap(),
        caller
    );
    release.send(()).unwrap();
    let result = finish(waiting).unwrap();
    assert_eq!(result.structured_content, answer().structured_content);
    assert_eq!(
        serde_json::to_value(result).unwrap()["content"][0]["text"],
        "selected queue entry"
    );
}

#[test]
fn a_control_error_retains_its_code_and_is_not_replayed() {
    let calls = Arc::new(AtomicUsize::new(0));
    let written = Arc::clone(&calls);
    let expected = tool_error(&Failure::invalid("queue revision changed"));
    let data = expected.data.clone();
    let message = expected.message.clone();
    let error = finish(perform(move || {
        written.fetch_add(1, Ordering::SeqCst);
        Err(expected)
    }))
    .unwrap_err();
    assert_eq!(error.message, message);
    assert_eq!(error.data, data);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn an_unpolled_request_never_starts_a_control() {
    let calls = Arc::new(AtomicUsize::new(0));
    let written = Arc::clone(&calls);
    let waiting = perform(move || {
        written.fetch_add(1, Ordering::SeqCst);
        Ok(answer())
    });
    drop(waiting);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[test]
fn dropping_an_inflight_request_does_not_replay_its_control() {
    let calls = Arc::new(AtomicUsize::new(0));
    let written = Arc::clone(&calls);
    let (started, running) = mpsc::channel();
    let (release, held) = mpsc::channel();
    let (completed, done) = mpsc::channel();
    let mut waiting = Box::pin(perform(move || {
        written.fetch_add(1, Ordering::SeqCst);
        started.send(()).unwrap();
        held.recv_timeout(Duration::from_secs(5)).unwrap();
        completed.send(()).unwrap();
        Ok(answer())
    }));
    let wake = waker();
    assert!(
        waiting
            .as_mut()
            .poll(&mut Context::from_waker(&wake))
            .is_pending()
    );
    running.recv_timeout(Duration::from_secs(5)).unwrap();
    drop(waiting);
    release.send(()).unwrap();
    done.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}
