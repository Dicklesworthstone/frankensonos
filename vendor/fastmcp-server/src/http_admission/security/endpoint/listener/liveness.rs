//! Caller-budget boundary for the entire native connection and TLS close.
//!
//! Request-read and per-frame write timeouts do not cover a JSON handler wait,
//! an idle SSE response, or a cleanup operation started near the caller's
//! deadline. Drive one owned future under the existing cancellation/deadline
//! guard, retaining it across every wake. Dropping it invokes the native
//! response/session owners; no detached task or second cleanup owner is created.

use std::future::Future;

use asupersync::Cx;

use super::super::{SecuredHttpEndpointError, await_dispatch};

pub(super) async fn drive<T>(
    cx: &Cx,
    operation: impl Future<Output = T>,
) -> Result<T, SecuredHttpEndpointError> {
    await_dispatch(cx, async { Ok(operation.await) }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::{pending, poll_fn};
    use std::io::{Read, Write};
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use std::task::{Context, Poll, Wake, Waker};
    use std::time::Duration;

    use fastmcp_core::McpRequestCancellation;
    use fastmcp_protocol::protocol_policy::ProtocolPolicy;

    use super::super::{BoundSecuredHttpServer, SecuredHttpIoLimits, connection, tls::ConnectionIo};
    use crate::{HttpListenerShutdown, Server};
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
    use crate::http_admission::security::HttpSecurityPolicy;

    struct WakeCount(AtomicUsize);
    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
        fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    struct Owner(Arc<AtomicUsize>);
    impl Drop for Owner {
        fn drop(&mut self) { self.0.fetch_add(1, Ordering::SeqCst); }
    }

    async fn bound(cx: &Cx) -> BoundSecuredHttpServer {
        let policy = HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 4096).unwrap()).unwrap(),
            "https://service.example", vec![],
        ).unwrap();
        Server::new("connection-budget-test", "1")
            .protocol_policy(ProtocolPolicy::ModernOnly).unwrap()
            .build().bind_secured_http(cx, "127.0.0.1:0", policy).await.unwrap()
    }

    fn assert_disconnected(peer: &mut std::net::TcpStream) {
        // This is an upper failure bound on observing an already-dropped
        // socket, not a sleep used to guess whether cancellation happened.
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut byte = [0_u8; 1];
        match peer.read(&mut byte) {
            Ok(0) => {},
            Err(error) if matches!(error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) => {},
            other => panic!("refused connection must close without response bytes: {other:?}"),
        }
    }

    #[test]
    fn caller_cancellation_closes_native_idle_and_partial_head_connections() {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(async {
                let cx = Cx::current().unwrap();
                let bound = bound(&cx).await;
                for prefix in [b"".as_slice(), b"POST /mcp HTTP/1.1\r\nHost:".as_slice()] {
                    let mut peer = std::net::TcpStream::connect(bound.local_addr().unwrap()).unwrap();
                    peer.write_all(prefix).unwrap();
                    let (stream, _) = bound.inner.listener.accept().await.unwrap();
                    let endpoint = Arc::clone(&bound.inner.endpoint);
                    let sessions = Arc::clone(&bound.inner.modern_sessions);
                    let policy = Arc::clone(&bound.policy);
                    let io = bound.io;
                    let (sender, mut receiver) = asupersync::channel::oneshot::channel::<Cx>();
                    let mut child = cx.spawn(move |child_cx| async move {
                        let connection = connection::serve(
                            &child_cx, ConnectionIo::Plain(stream), endpoint, sessions,
                            HttpListenerShutdown::new(&child_cx), policy, io,
                        );
                        let mut driving = Box::pin(drive(&child_cx, connection));
                        let mut sender = Some(sender);
                        poll_fn(|task| {
                            let result = driving.as_mut().poll(task);
                            if result.is_pending() && let Some(sender) = sender.take() {
                                // A positive handshake proves the native ingress
                                // is parked before the caller cancels its child.
                                let _ = sender.send_blocking(child_cx.clone());
                            }
                            result
                        }).await
                    }).unwrap();
                    let child_cx = receiver.recv(&cx).await.unwrap();
                    // Cancel with an attributed reason, as the runtime does.
                    // A reasonless `set_cancel_requested` makes asupersync 0.5
                    // discard the task's value at `join` ("join channel
                    // closed"), hiding the refusal this test observes.
                    child_cx.cancel_with(
                        asupersync::types::CancelKind::User,
                        Some("caller cancelled the connection"),
                    );
                    let result = asupersync::time::timeout(
                        cx.now(), Duration::from_secs(1), child.join(&cx),
                    ).await.expect("cancelled connection must settle").unwrap();
                    assert_eq!(result, Err(SecuredHttpEndpointError::Cancelled));
                    assert!(cx.checkpoint().is_ok(), "a connection must not cancel its listener");
                    assert_disconnected(&mut peer);
                    assert!(bound.inner.modern_sessions.sessions.lock().unwrap().is_empty());
                }
            });
    }

    #[test]
    fn unserviceable_caller_deadline_drops_native_socket_before_ingress() {
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(async {
                let runtime_cx = Cx::current().unwrap();
                let bound = bound(&runtime_cx).await;
                let mut peer = std::net::TcpStream::connect(bound.local_addr().unwrap()).unwrap();
                let (stream, _) = bound.inner.listener.accept().await.unwrap();
                let cx = Cx::for_testing_with_budget(asupersync::Budget::INFINITE.with_deadline(
                    asupersync::Time::ZERO.saturating_add_nanos(u64::MAX),
                ));
                assert!(cx.timer_driver().is_none());
                // Box the connection future rather than holding it inline. It
                // embeds `ingress::receive`, so it is large, and the two other
                // `connection::serve` call sites already box it -- the
                // production path at listener.rs:287 does so through an
                // explicit `Pin<Box<dyn Future>>`. This was the only site
                // keeping it on the stack.
                let connection = Box::pin(connection::serve(
                    &cx,
                    ConnectionIo::Plain(stream),
                    Arc::clone(&bound.inner.endpoint),
                    Arc::clone(&bound.inner.modern_sessions),
                    HttpListenerShutdown::new(&cx),
                    Arc::clone(&bound.policy),
                    bound.io,
                ));
                let result = drive(&cx, connection).await;
                assert_eq!(result, Err(SecuredHttpEndpointError::TimerUnavailable));
                assert_disconnected(&mut peer);
                assert!(bound.inner.modern_sessions.sessions.lock().unwrap().is_empty());
                assert!(runtime_cx.checkpoint().is_ok());
            });
    }

    #[test]
    fn driverless_caller_still_times_out_an_idle_request_head() {
        // bd-if2ni: asupersync parks a timeout without a wake only through
        // `poll_with_time`. An awaited timeout polls its Sleep, which falls
        // back to the process-global timer, so the request timeout must fire
        // for a caller Cx with no timer driver. The second pass differs only
        // in owning a driver.
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(async {
                let runtime_cx = Cx::current().unwrap();
                let bound = bound(&runtime_cx).await;
                let io = SecuredHttpIoLimits::new(Duration::from_millis(50), Duration::from_secs(1))
                    .unwrap();
                for with_driver in [false, true] {
                    let cx = if with_driver { runtime_cx.clone() } else { Cx::for_testing() };
                    assert_eq!(cx.timer_driver().is_some(), with_driver);
                    let mut peer = std::net::TcpStream::connect(bound.local_addr().unwrap()).unwrap();
                    let (stream, _) = bound.inner.listener.accept().await.unwrap();
                    let connection = Box::pin(connection::serve(
                        &cx,
                        ConnectionIo::Plain(stream),
                        Arc::clone(&bound.inner.endpoint),
                        Arc::clone(&bound.inner.modern_sessions),
                        HttpListenerShutdown::new(&cx),
                        Arc::clone(&bound.policy),
                        io,
                    ));
                    // The outer bound runs on the driver-owning runtime Cx, so
                    // an inner timeout that never wakes fails here, not hangs.
                    let result = asupersync::time::timeout(
                        runtime_cx.now(), Duration::from_secs(5), drive(&cx, connection),
                    ).await.expect("an idle request head must time out");
                    assert_eq!(result, Ok(()));
                    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
                    let mut response = Vec::new();
                    let _ = peer.read_to_end(&mut response);
                    assert!(response.starts_with(b"HTTP/1.1 408"),
                        "with_driver={with_driver}: {:?}", String::from_utf8_lossy(&response));
                }
            });
    }

    #[test]
    fn cancelled_pending_connection_drops_custody_without_restarting_or_cancelling_siblings() {
        let cx = Cx::for_testing();
        let sibling = Cx::for_testing();
        let dropped = Arc::new(AtomicUsize::new(0));
        let owner = Owner(Arc::clone(&dropped));
        let started = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&started);
        let operation = async move {
            let _owner = owner;
            observed.fetch_add(1, Ordering::SeqCst);
            pending::<()>().await;
        };
        let mut driving = Box::pin(drive(&cx, operation));
        let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&counter));
        let mut task = Context::from_waker(&waker);
        for _ in 0..3 { assert!(driving.as_mut().poll(&mut task).is_pending()); }
        assert_eq!(started.load(Ordering::SeqCst), 1, "a partially driven operation must never restart");
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
        let wakes_before = counter.0.load(Ordering::SeqCst);
        cx.set_cancel_requested(true);
        assert!(counter.0.load(Ordering::SeqCst) > wakes_before,
            "an idle connection must wake without another I/O event");
        assert_eq!(driving.as_mut().poll(&mut task), Poll::Ready(Err(SecuredHttpEndpointError::Cancelled)));
        assert_eq!(dropped.load(Ordering::SeqCst), 1, "native custody must drop before returning refusal");
        assert!(sibling.checkpoint().is_ok());
    }

    #[test]
    fn abandoning_connection_driver_drops_custody_without_cancelling_its_caller() {
        let cx = Cx::for_testing();
        let dropped = Arc::new(AtomicUsize::new(0));
        let owner = Owner(Arc::clone(&dropped));
        let mut driving = Box::pin(drive(&cx, async move {
            let _owner = owner;
            pending::<()>().await;
        }));
        let mut task = Context::from_waker(Waker::noop());
        assert!(driving.as_mut().poll(&mut task).is_pending());
        drop(driving);
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
        assert!(cx.checkpoint().is_ok());
    }

    #[test]
    fn request_token_cancellation_does_not_suppress_a_committed_terminal_drain() {
        let cx = Cx::for_testing();
        let request = McpRequestCancellation::new();
        request.cancel();
        let mut driving = Box::pin(drive(&cx, async {
            assert!(request.is_cancel_requested());
            // The native response state machine, not request-token liveness,
            // determines whether a graceful terminal still needs delivery.
            1_u8
        }));
        let mut task = Context::from_waker(Waker::noop());
        assert_eq!(driving.as_mut().poll(&mut task), Poll::Ready(Ok(1)));
        assert!(cx.checkpoint().is_ok());
    }

    #[test]
    fn cancellation_during_connection_completion_withholds_and_drops_the_result() {
        let cx = Cx::for_testing();
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut driving = Box::pin(drive(&cx, poll_fn(|_| {
            cx.set_cancel_requested(true);
            Poll::Ready(Owner(Arc::clone(&dropped)))
        })));
        let mut task = Context::from_waker(Waker::noop());
        assert!(matches!(driving.as_mut().poll(&mut task),
            Poll::Ready(Err(SecuredHttpEndpointError::Cancelled))));
        assert_eq!(dropped.load(Ordering::SeqCst), 1);
    }
}
