//! Native identity callbacks must not occupy asynchronous listener workers.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::ThreadId;
use std::time::{Duration, Instant};

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpError};
use fastmcp_transport::http::{HttpMethod, HttpRequest, HttpStatus};

use crate::http_identity::MAX_IDENTITY_JOBS;
use crate::router::TransportAuthorization;
use crate::{HttpServerConfig, Server};

struct Gate {
    caller: ThreadId,
    entered: AtomicUsize,
    open: Mutex<bool>,
    changed: Condvar,
}

impl Gate {
    fn new() -> Self {
        Self {
            caller: std::thread::current().id(),
            entered: AtomicUsize::new(0),
            open: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    fn resolve(&self) -> AuthContext {
        assert_ne!(
            std::thread::current().id(),
            self.caller,
            "an identity callback must not execute on the current-thread I/O runtime",
        );
        self.entered.fetch_add(1, Ordering::SeqCst);
        let (open, _) = self
            .changed
            .wait_timeout_while(self.open.lock().unwrap(), Duration::from_secs(5), |open| {
                !*open
            })
            .unwrap();
        assert!(*open, "the test releases held callbacks");
        AuthContext::with_subject("alice")
    }

    fn release(&self) {
        *self
            .open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.changed.notify_all();
    }
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn request() -> HttpRequest {
    HttpRequest::new(HttpMethod::Post, "/mcp")
        .with_header("x-test-header", "preserved")
        .with_body(vec![b'x'; 1024])
}

fn runtime() -> asupersync::runtime::Runtime {
    asupersync::runtime::RuntimeBuilder::current_thread()
        .with_reactor(
            asupersync::runtime::reactor::create_reactor()
                .expect("identity admission test reactor"),
        )
        .build()
        .expect("identity admission test runtime")
}

async fn wait_until(cx: &Cx, done: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !done() {
        assert!(
            Instant::now() < deadline,
            "identity workers did not reach the barrier"
        );
        asupersync::time::sleep(cx.now(), Duration::from_millis(1)).await;
    }
}

#[test]
fn held_identity_jobs_leave_io_live_and_keep_their_slots_after_cancellation() {
    let gate = Arc::new(Gate::new());
    let _release = Release(Arc::clone(&gate));
    let callback_gate = Arc::clone(&gate);
    let server = Arc::new(
        Server::new("bounded-identity", "1")
            .http_config(
                HttpServerConfig::new().identity_resolver(move |peer, headers| {
                    assert_eq!(peer, "127.0.0.1:32123".parse().unwrap());
                    assert_eq!(
                        headers,
                        &[("x-test-header".to_owned(), "preserved".to_owned())],
                        "only the actual request headers cross to the callback",
                    );
                    Ok(callback_gate.resolve())
                }),
            )
            .build(),
    );
    let pending = Arc::clone(
        &server
            .http_config
            .identity_resolver
            .as_ref()
            .unwrap()
            .pending,
    );
    runtime().block_on(async move {
        let cx = Cx::current().expect("identity admission context");
        let mut jobs = Vec::new();
        for _ in 0..MAX_IDENTITY_JOBS {
            let server = Arc::clone(&server);
            jobs.push(
                cx.spawn(move |job_cx| async move {
                    server
                        .resolve_native_http_identity_async(
                            &job_cx,
                            "127.0.0.1:32123".parse().unwrap(),
                            &request(),
                            TransportAuthorization::default(),
                        )
                        .await
                })
                .expect("identity request task"),
            );
        }
        wait_until(&cx, || {
            gate.entered.load(Ordering::SeqCst) == MAX_IDENTITY_JOBS
        })
        .await;
        assert_eq!(pending.load(Ordering::SeqCst), MAX_IDENTITY_JOBS);

        // Another listener can admit and resolve a caller while all eight
        // callbacks above are held. No caller-owned blocking pool is required.
        let other = Server::new("independent-identity", "1")
            .http_config(
                HttpServerConfig::new()
                    .identity_resolver(|_, _| Ok(AuthContext::with_subject("bob"))),
            )
            .build();
        let admitted = other
            .resolve_native_http_identity_async(
                &cx,
                "127.0.0.1:32123".parse().unwrap(),
                &request(),
                TransportAuthorization::default(),
            )
            .await
            .expect("an independent identity lookup must keep progressing");
        assert_eq!(admitted.identity().unwrap().subject.as_deref(), Some("bob"));

        jobs[0].abort();
        asupersync::runtime::yield_now().await;
        let overloaded = server
            .resolve_native_http_identity_async(
                &cx,
                "127.0.0.1:32123".parse().unwrap(),
                &request(),
                TransportAuthorization::default(),
            )
            .await;
        assert!(matches!(
            overloaded,
            Err(response) if response.status == HttpStatus::SERVICE_UNAVAILABLE
                && response.body.is_empty()
        ));
        assert_eq!(pending.load(Ordering::SeqCst), MAX_IDENTITY_JOBS);
        assert_eq!(
            gate.entered.load(Ordering::SeqCst),
            MAX_IDENTITY_JOBS,
            "overload and a dropped waiter must not re-enter the resolver",
        );

        gate.release();
        for (index, mut job) in jobs.into_iter().enumerate() {
            let answer = job.join(&cx).await;
            if index == 0 {
                assert!(
                    answer.is_err(),
                    "the cancelled waiter cannot receive identity custody"
                );
            } else {
                let authorization = answer
                    .expect("identity task joins")
                    .expect("held identity callback succeeds");
                assert_eq!(
                    authorization.identity().unwrap().subject.as_deref(),
                    Some("alice"),
                );
            }
        }
        wait_until(&cx, || pending.load(Ordering::SeqCst) == 0).await;
        let next = server
            .resolve_native_http_identity_async(
                &cx,
                "127.0.0.1:32123".parse().unwrap(),
                &request(),
                TransportAuthorization::default(),
            )
            .await
            .expect("a slot is reusable after the actual callback returns");
        assert_eq!(next.identity().unwrap().subject.as_deref(), Some("alice"));
        assert_eq!(gate.entered.load(Ordering::SeqCst), MAX_IDENTITY_JOBS + 1);
    });
}

#[test]
fn async_identity_failures_and_mixed_credentials_never_gain_fallback_authority() {
    runtime().block_on(async move {
        let cx = Cx::current().expect("identity admission context");
        for failure in ["error", "panic", "oversized"] {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);
            let server = Server::new("bounded-identity-failure", "1")
                .http_config(HttpServerConfig::new().identity_resolver(move |_, _| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    match failure {
                        "error" => Err(McpError::internal_error("lookup failed")),
                        "panic" => panic!("resolver panic"),
                        _ => Ok(AuthContext::with_subject("x".repeat(1025))),
                    }
                }))
                .build();
            let answer = server
                .resolve_native_http_identity_async(
                    &cx,
                    "127.0.0.1:32123".parse().unwrap(),
                    &request(),
                    TransportAuthorization::default(),
                )
                .await;
            assert!(matches!(
                answer,
                Err(response) if response.status == HttpStatus::FORBIDDEN
                    && response.body.is_empty()
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                server
                    .http_config
                    .identity_resolver
                    .as_ref()
                    .unwrap()
                    .pending
                    .load(Ordering::SeqCst),
                0,
            );
            let mixed = server
                .resolve_native_http_identity_async(
                    &cx,
                    "127.0.0.1:32123".parse().unwrap(),
                    &request(),
                    TransportAuthorization::from_singleton_header(Some("Bearer untrusted")),
                )
                .await;
            assert!(matches!(
                mixed,
                Err(response) if response.status == HttpStatus::FORBIDDEN
            ));
            assert_eq!(calls.load(Ordering::SeqCst), 1);
        }
    });
}
