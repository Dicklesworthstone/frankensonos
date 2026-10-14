//! Origin-bound embedding entry point for the shipped HTTP dispatcher.
//!
//! Security refusals, browser preflights and explicitly configured resource
//! metadata GETs complete before a server session is opened. Actual POSTs use
//! the existing authentication, authorization, strict protocol decoder and
//! dispatch path. No credential or request header is removed to evade a
//! downstream policy. The server's configured policy may be stricter.
//!
//! This is explicit embedding, not automatic installation on `serve_http`.
//! Write the returned response head, not the original head retained privately by
//! an SSE body. The SSE owner retains its session until explicit async close or
//! drop; returning a stream never drops the session that owns its dispatch.

/// Socket-to-dispatch security for the caller-owned native HTTP listener.
pub mod listener;
/// Explicit bounded credential revalidation for secured SSE responses.
pub mod revalidation;
mod scope;

use std::future::{Future, poll_fn};
use std::task::Poll;
use std::time::Duration;

use asupersync::{Cx, channel::oneshot, time::Sleep};
use fastmcp_transport::{http::{HttpRequest, HttpResponse}, sse::SseEvent};

use revalidation::{SseAuthorizationError, SseAuthorizationLease};
use super::{CorsResponseHeaders, HttpSecurityError, HttpSecurityHead, HttpSecurityPolicy};
use crate::{ServerHttpEndpoint, ServerHttpEndpointResponse, ServerHttpSession, ServerHttpSseResponse};

/// Response bytes/head plus an optional native SSE body with its owning session.
/// An immediate response carries its buffered body; a stream carries it in the
/// second part. No polling task or queue is hidden behind this owner.
#[must_use = "write the response and asynchronously close any returned SSE owner"]
pub struct SecuredHttpEndpointResponse {
    response: HttpResponse,
    stream: Option<SecuredHttpSseResponse>,
}

impl SecuredHttpEndpointResponse {
    pub fn response(&self) -> &HttpResponse { &self.response }
    pub fn is_streaming(&self) -> bool { self.stream.is_some() }

    /// Write this head before driving the stream on the caller's Cx. Its CORS
    /// headers are authoritative; the native body's old response head is not.
    pub fn into_parts(self) -> (HttpResponse, Option<SecuredHttpSseResponse>) {
        (self.response, self.stream)
    }

    fn immediate(response: HttpResponse) -> Self { Self { response, stream: None } }
}

/// A native SSE response and the session owning its dispatch. Call `close`
/// after terminal delivery or peer disconnect to join dispatch tasks on the
/// caller's runtime. Drop cancels but cannot synchronously join async work.
/// Revalidating responses expose events only through `next_event`; no raw body
/// reference can bypass their credential checks.
#[must_use = "close the SSE owner asynchronously after driving its response body"]
pub struct SecuredHttpSseResponse {
    // Declaration order deliberately drops the response before its session.
    stream: Option<Box<ServerHttpSseResponse>>,
    session: Option<ServerHttpSession>,
    authorization: Option<SseAuthorizationLease>,
    finished: bool,
}

impl SecuredHttpSseResponse {
    /// Access the old native body API only for responses without revalidation.
    /// Revalidating or already-closed responses return None; use `next_event`
    /// instead. This prevents a host from accidentally bypassing the guard.
    pub fn stream(&mut self) -> Option<&mut ServerHttpSseResponse> {
        if self.authorization.is_some() { return None; }
        self.stream.as_deref_mut()
    }

    /// Delivers one native SSE event after checking the opening credential.
    /// Idle waits also revalidate. A dropped, polled read owns and drops the
    /// native response rather than leaving a partially consumed body reusable.
    /// Only a delivered terminal response permits a later successful None.
    /// The host must close on error; an already-written 200 head cannot become
    /// a new authentication challenge, and no success terminal is fabricated.
    pub async fn next_event(&mut self, cx: &Cx) -> Result<Option<SseEvent>, SecuredHttpEndpointError> {
        if self.finished { return Ok(None); }
        let stream = self.stream.take().ok_or(SecuredHttpEndpointError::BodyClosed)?;
        let mut authorization = self.authorization.take();
        let event = guard_response(cx, &mut authorization, async {
            loop {
                checkpoint(cx)?;
                match stream.pop_event() {
                    Ok(Some(event)) => return Ok::<_, SecuredHttpEndpointError>(event),
                    Ok(None) if !stream.is_finished() => {},
                    Ok(None) => return Err(SecuredHttpEndpointError::BodyClosed),
                    Err(_) => return Err(SecuredHttpEndpointError::BodyFailed),
                }
                if cx.timer_driver().is_none() { return Err(SecuredHttpEndpointError::TimerUnavailable); }
                asupersync::time::sleep(cx.now(), Duration::from_millis(10)).await;
            }
        }).await??;
        self.finished = crate::final_subscription_terminal_response_event(&event);
        self.stream = Some(stream);
        self.authorization = authorization;
        Ok(Some(event))
    }

    /// Release the body and credential custody, then join session dispatch. A
    /// dropped close future drops the local session and invokes its fallback.
    pub async fn close(&mut self, cx: &Cx) {
        self.stream = None;
        self.authorization = None;
        if let Some(mut session) = self.session.take() { session.close(cx).await; }
    }
}

/// Fixed diagnostics intentionally do not retain lower-layer request/peer text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecuredHttpEndpointError {
    PolicyRouteMismatch,
    Cancelled,
    TimedOut,
    TimerUnavailable,
    SessionUnavailable,
    DispatchFailed,
    UnexpectedLegacyStream,
    BodyClosed,
    BodyFailed,
    Revalidation(SseAuthorizationError),
}

impl std::fmt::Display for SecuredHttpEndpointError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::PolicyRouteMismatch => "HTTP security policy does not match the server route",
            Self::Cancelled => "secured HTTP dispatch cancelled",
            Self::TimedOut => "secured HTTP dispatch deadline expired",
            Self::TimerUnavailable => "secured HTTP dispatch requires the caller's deadline timer",
            Self::SessionUnavailable => "secured HTTP session could not be opened",
            Self::DispatchFailed => "secured HTTP dispatch failed",
            Self::UnexpectedLegacyStream => "modern secured HTTP dispatch produced a legacy stream",
            Self::BodyClosed => "secured SSE response ended without a delivered terminal",
            Self::BodyFailed => "secured SSE response failed",
            Self::Revalidation(error) => return std::fmt::Display::fmt(error, f),
        })
    }
}
impl std::error::Error for SecuredHttpEndpointError {}

impl ServerHttpEndpoint {
    /// Handles one stateless modern POST, browser preflight, or configured
    /// resource-metadata GET through an administrator-selected origin policy.
    /// No runtime is created. The caller's Cx deadline and cancellation span
    /// session opening and awaited dispatch.
    ///
    /// After head/body security checks, POSTs retain the native strict protocol
    /// and authentication boundaries. An installed HTTP scope policy runs on
    /// verified facts before transport admission or SSE allocation; its native
    /// 401/403 is not inferred from an application JSON-RPC error. Optional SSE
    /// revalidation retains that exact request, provider and opening credential.
    /// Preflight and public metadata never open a session or authenticate.
    ///
    /// The supplied policy must describe the server's configured modern path.
    /// Existing server CORS/authorization policy is still enforced and can refuse
    /// a POST even after preflight. Configure both from the same deployment policy.
    /// The returned SSE owner retains its native session and request lifetime.
    pub async fn handle_secured_async(
        &self,
        cx: &Cx,
        policy: &HttpSecurityPolicy,
        request: HttpRequest,
    ) -> Result<SecuredHttpEndpointResponse, SecuredHttpEndpointError> {
        checkpoint(cx)?;
        if self.server.configured_http_request_handler().config().base_path != policy.endpoint().path() {
            return Err(SecuredHttpEndpointError::PolicyRouteMismatch);
        }
        let prepared = prepare_request(policy, &request);
        checkpoint(cx)?;
        let cors = match prepared {
            PreparedRequest::Immediate(response) => return Ok(SecuredHttpEndpointResponse::immediate(response)),
            PreparedRequest::Post(cors) => cors,
        };
        let ((response, authorization), mut session) = Box::pin(await_dispatch(cx, async {
            let mut session = self.open_session(cx).map_err(|_| SecuredHttpEndpointError::SessionUnavailable)?;
            let dispatched = match &policy.scope_authorization {
                Some(scopes) => Box::pin(scope::dispatch(&mut session, cx, scopes, request, policy.sse_revalidation)).await,
                None => session.handle_async(cx, request).await
                    .map(|response| (response, None))
                    .map_err(|_| SecuredHttpEndpointError::DispatchFailed),
            };
            match dispatched {
                Ok(response) => Ok((response, session)),
                Err(error) => {
                    session.close(cx).await;
                    Err(error)
                }
            }
        })).await?;
        let response = match response {
            ServerHttpEndpointResponse::Immediate(mut response) => {
                drop(authorization);
                await_dispatch(cx, async { session.close(cx).await; Ok(()) }).await?;
                cors.apply_to(&mut response);
                SecuredHttpEndpointResponse::immediate(response)
            }
            ServerHttpEndpointResponse::ModernSse(stream) => {
                let mut response = stream.response().clone();
                cors.apply_to(&mut response);
                SecuredHttpEndpointResponse {
                    response,
                    stream: Some(SecuredHttpSseResponse {
                        stream: Some(Box::new(stream)), session: Some(session), authorization, finished: false,
                    }),
                }
            }
            #[allow(unreachable_patterns)]
            other => {
                drop(other);
                drop(authorization);
                await_dispatch(cx, async { session.close(cx).await; Ok(()) }).await?;
                return Err(SecuredHttpEndpointError::UnexpectedLegacyStream);
            }
        };
        checkpoint(cx)?;
        Ok(response)
    }
}

// Poll one owned operation without restarting it. The finite timer wakes an
// idle response, a pending representation election, or a blocked socket write.
// Every resume checks before and after polling so a ready event cannot bypass
// a due refusal. The surrounding response owner performs cancellation/cleanup.
async fn guard_response<T>(
    cx: &Cx,
    authorization: &mut Option<SseAuthorizationLease>,
    future: impl Future<Output = T>,
) -> Result<T, SecuredHttpEndpointError> {
    let Some(lease) = authorization.as_mut() else {
        // Credential revalidation is optional; execution liveness is not.
        // Register cancellation/deadline wakes even when the operation itself
        // is idle, and refuse a result that becomes ready during cancellation.
        return await_dispatch(cx, async { Ok(future.await) }).await;
    };
    if cx.timer_driver().is_none() { return Err(SecuredHttpEndpointError::TimerUnavailable); }
    let mut wake = Box::pin(Sleep::new(cx.now().saturating_add_nanos(10_000_000)));
    let mut future = std::pin::pin!(future);
    poll_fn(|task| {
        let _current = Cx::set_current(Some(cx.clone()));
        lease.check(cx).map_err(SecuredHttpEndpointError::Revalidation)?;
        let result = future.as_mut().poll(task);
        lease.check(cx).map_err(SecuredHttpEndpointError::Revalidation)?;
        if let Poll::Ready(value) = result { return Poll::Ready(Ok(value)); }
        if wake.as_mut().poll(task).is_ready() {
            wake = Box::pin(Sleep::new(cx.now().saturating_add_nanos(10_000_000)));
            let _ = wake.as_mut().poll(task);
        }
        Poll::Pending
    }).await
}

enum PreparedRequest {
    Immediate(HttpResponse),
    Post(CorsResponseHeaders),
}

fn prepare_request(policy: &HttpSecurityPolicy, request: &HttpRequest) -> PreparedRequest {
    // Preserve differently-cased map entries; wire adapters must have rejected
    // duplicates that would otherwise be lost before constructing this map.
    let limits = policy.endpoint().limits();
    let rejection = if let Err(error) = policy.admit_route(request.method.as_str(), &request.path) {
        Some(error)
    } else if policy.is_metadata_path(&request.path) && !request.query.is_empty() {
        Some(HttpSecurityError::EndpointMismatch)
    } else if request.headers.len() > limits.max_header_count()
        || request.headers.iter().fold(0_usize, |size, (name, value)|
            size.saturating_add(name.len()).saturating_add(value.len())) > limits.max_header_block_bytes()
    {
        Some(HttpSecurityError::HeaderLimit)
    } else { None };
    if let Some(rejection) = rejection { return PreparedRequest::Immediate(rejection.response()); }
    let headers: Vec<_> = request.headers.iter().map(|(name, value)| (name.clone(), value.clone())).collect();
    let head = match policy.admit_head(request.method.as_str(), &request.path, &headers) {
        Ok(head) => head,
        Err(error) => return PreparedRequest::Immediate(error.response()),
    };
    if let Err(error) = policy.validate_body(!matches!(&head, HttpSecurityHead::Post(_)), &headers, &request.body) {
        let mut response = error.response();
        if let HttpSecurityHead::Post(cors) = head { cors.apply_to(&mut response); }
        return PreparedRequest::Immediate(response);
    }
    match head {
        HttpSecurityHead::Preflight(response) | HttpSecurityHead::Metadata(response) => PreparedRequest::Immediate(response),
        HttpSecurityHead::Post(cors) => PreparedRequest::Post(cors),
    }
}

fn checkpoint(cx: &Cx) -> Result<(), SecuredHttpEndpointError> {
    cx.checkpoint().map_err(|_| SecuredHttpEndpointError::Cancelled)?;
    if cx.budget().deadline.is_some_and(|deadline| cx.now() >= deadline) {
        return Err(SecuredHttpEndpointError::TimedOut);
    }
    Ok(())
}

async fn await_dispatch<T>(
    cx: &Cx,
    future: impl Future<Output = Result<T, SecuredHttpEndpointError>>,
) -> Result<T, SecuredHttpEndpointError> {
    let deadline = cx.budget().deadline;
    if deadline.is_some() && cx.timer_driver().is_none() { return Err(SecuredHttpEndpointError::TimerUnavailable); }
    let mut timeout = deadline.map(|deadline| Box::pin(Sleep::new(deadline)));
    let (_sender, mut receiver) = oneshot::channel::<()>();
    let mut cancelled = std::pin::pin!(receiver.recv(cx));
    let mut future = std::pin::pin!(future);
    poll_fn(|task| {
        checkpoint(cx)?;
        let _current = Cx::set_current(Some(cx.clone()));
        if cancelled.as_mut().poll(task).is_ready() { return Poll::Ready(Err(SecuredHttpEndpointError::Cancelled)); }
        if timeout.as_mut().is_some_and(|timeout| timeout.as_mut().poll(task).is_ready()) {
            return Poll::Ready(Err(SecuredHttpEndpointError::TimedOut));
        }
        let result = future.as_mut().poll(task);
        checkpoint(cx)?;
        result
    }).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_admission::{HttpAdmissionLimits, HttpEndpointConfig};
    use fastmcp_transport::http::HttpMethod;

    fn policy() -> HttpSecurityPolicy {
        HttpSecurityPolicy::new(
            HttpEndpointConfig::new("/mcp", HttpAdmissionLimits::new(32, 8192, 4096).unwrap()).unwrap(),
            "https://service.example", vec!["https://app.example".to_owned()],
        ).unwrap()
    }

    #[test]
    fn buffered_preflight_is_an_immediate_response_without_a_dispatch_permit() {
        let request = HttpRequest::new(HttpMethod::Options, "/mcp")
            .with_header("host", "service.example")
            .with_header("origin", "https://app.example")
            .with_header("access-control-request-method", "POST")
            .with_header("access-control-request-headers", "mcp-method, authorization");
        let PreparedRequest::Immediate(response) = prepare_request(&policy(), &request)
            else { panic!("preflight must never dispatch") };
        assert_eq!(response.status.0, 204);
        assert!(response.body.is_empty());
        let request = request.with_body(b"not empty".to_vec());
        let PreparedRequest::Immediate(response) = prepare_request(&policy(), &request)
            else { panic!("body must not dispatch") };
        assert_eq!(response.status.0, 400);
    }

    #[test]
    fn security_refusal_precedes_json_while_json_errors_stay_with_native_dispatch() {
        let mut request = HttpRequest::new(HttpMethod::Post, "/mcp")
            .with_header("host", "service.example")
            .with_header("origin", "https://app.example")
            .with_body(b"malformed JSON".to_vec());
        assert!(matches!(prepare_request(&policy(), &request), PreparedRequest::Post(_)));
        request.headers.insert("origin".to_owned(), "https://attacker.example".to_owned());
        let PreparedRequest::Immediate(response) = prepare_request(&policy(), &request)
            else { panic!("origin must not dispatch") };
        assert_eq!(response.status.0, 403);
        assert!(!response.headers.contains_key("access-control-allow-origin"));
    }

    #[test]
    fn map_bounds_duplicate_casing_and_body_bounds_do_not_allocate_a_session() {
        let request = HttpRequest::new(HttpMethod::Post, "/mcp").with_header("host", "service.example");
        let mut duplicate = request.clone();
        duplicate.headers.insert("HOST".to_owned(), "service.example".to_owned());
        let PreparedRequest::Immediate(response) = prepare_request(&policy(), &duplicate)
            else { panic!("duplicate must not dispatch") };
        assert_eq!(response.status.0, 400);
        let oversized = request.with_body(vec![b'x'; 4097]);
        let PreparedRequest::Immediate(response) = prepare_request(&policy(), &oversized)
            else { panic!("oversized body must not dispatch") };
        assert_eq!(response.status.0, 413);
    }

    #[test]
    fn abandoning_a_polled_guard_releases_the_owned_dispatch_future() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        struct Pending(Arc<AtomicBool>);
        impl Future for Pending {
            type Output = Result<(), SecuredHttpEndpointError>;
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<Self::Output> { Poll::Pending }
        }
        impl Drop for Pending { fn drop(&mut self) { self.0.store(true, Ordering::Release); } }
        asupersync::runtime::RuntimeBuilder::current_thread()
            .with_reactor(asupersync::runtime::reactor::create_reactor().unwrap())
            .build().unwrap().block_on(async {
                let cx = Cx::current().unwrap();
                let dropped = Arc::new(AtomicBool::new(false));
                let mut waiting = Box::pin(await_dispatch(&cx, Pending(Arc::clone(&dropped))));
                poll_fn(|task| { assert!(waiting.as_mut().poll(task).is_pending()); Poll::Ready(()) }).await;
                drop(waiting);
                assert!(dropped.load(Ordering::Acquire));
                assert!(cx.checkpoint().is_ok());
            });
    }

    #[test]
    fn buffered_metadata_is_immediate_and_cannot_select_a_resource_by_query() {
        use super::super::resource_metadata::ProtectedResourceMetadata;
        let policy = policy().with_resource_metadata(ProtectedResourceMetadata::new(
            vec!["https://issuer.example".to_owned()],
        ).unwrap()).unwrap();
        let request = HttpRequest::new(HttpMethod::Get, policy.resource_metadata_path().unwrap())
            .with_header("host", "service.example");
        let PreparedRequest::Immediate(response) = prepare_request(&policy, &request)
            else { panic!("metadata must never dispatch") };
        assert_eq!(response.status.0, 200);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&response.body).unwrap()["resource"], "https://service.example/mcp");
        let mut with_query = request.clone();
        with_query.query.insert("resource".to_owned(), "https://attacker.example".to_owned());
        let PreparedRequest::Immediate(response) = prepare_request(&policy, &with_query)
            else { panic!("query must never dispatch") };
        assert_eq!(response.status.0, 404);
        let PreparedRequest::Immediate(response) = prepare_request(&policy, &request.with_body(b"x".to_vec()))
            else { panic!("body must never dispatch") };
        assert_eq!(response.status.0, 400);
    }
    #[test]
    fn response_guard_without_revalidation_preserves_live_ready_results() {
        let cx = Cx::for_testing();
        assert!(cx.timer_driver().is_none());
        let mut authorization = None;
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, async { 17 }));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert_eq!(guarded.as_mut().poll(&mut task), Poll::Ready(Ok(17)));
    }

    #[test]
    fn response_guard_without_revalidation_refuses_cancel_before_polling() {
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);
        let polls = std::cell::Cell::new(0);
        let mut authorization = None;
        let operation = poll_fn(|_| {
            polls.set(polls.get() + 1);
            Poll::Ready(17)
        });
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, operation));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert_eq!(guarded.as_mut().poll(&mut task), Poll::Ready(Err(SecuredHttpEndpointError::Cancelled)));
        assert_eq!(polls.get(), 0, "cancelled response must not consume an event or write bytes");
    }

    #[test]
    fn response_guard_without_revalidation_refuses_an_unserviceable_deadline() {
        let cx = Cx::for_testing_with_budget(
            asupersync::Budget::INFINITE.with_deadline(
                asupersync::Time::ZERO.saturating_add_nanos(u64::MAX),
            ),
        );
        assert!(cx.timer_driver().is_none());
        let polls = std::cell::Cell::new(0);
        let mut authorization = None;
        let operation = poll_fn(|_| {
            polls.set(polls.get() + 1);
            Poll::Ready(17)
        });
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, operation));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert_eq!(guarded.as_mut().poll(&mut task), Poll::Ready(Err(SecuredHttpEndpointError::TimerUnavailable)));
        assert_eq!(polls.get(), 0, "an unenforceable deadline must not start response work");
    }

    #[test]
    fn response_guard_without_revalidation_withholds_a_ready_result_on_cancel() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        struct ResultOwner(Arc<AtomicBool>);
        impl Drop for ResultOwner {
            fn drop(&mut self) { self.0.store(true, Ordering::Release); }
        }
        let cx = Cx::for_testing();
        let dropped = Arc::new(AtomicBool::new(false));
        let mut authorization = None;
        let operation = poll_fn(|_| {
            cx.set_cancel_requested(true);
            Poll::Ready(ResultOwner(Arc::clone(&dropped)))
        });
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, operation));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(matches!(guarded.as_mut().poll(&mut task), Poll::Ready(Err(SecuredHttpEndpointError::Cancelled))));
        assert!(dropped.load(Ordering::Acquire), "withheld result must release its owned resources");
    }

    #[test]
    fn response_guard_without_revalidation_wakes_an_idle_operation_on_cancel() {
        use std::sync::{Arc, atomic::{AtomicBool, AtomicUsize, Ordering}};
        struct WakeCount(AtomicUsize);
        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) { self.0.fetch_add(1, Ordering::Relaxed); }
            fn wake_by_ref(self: &Arc<Self>) { self.0.fetch_add(1, Ordering::Relaxed); }
        }
        struct Idle(Arc<AtomicBool>);
        impl Future for Idle {
            type Output = ();
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<()> { Poll::Pending }
        }
        impl Drop for Idle {
            fn drop(&mut self) { self.0.store(true, Ordering::Release); }
        }
        let cx = Cx::for_testing();
        let counter = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = std::task::Waker::from(Arc::clone(&counter));
        let mut task = std::task::Context::from_waker(&waker);
        let dropped = Arc::new(AtomicBool::new(false));
        let mut authorization = None;
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, Idle(Arc::clone(&dropped))));
        assert!(guarded.as_mut().poll(&mut task).is_pending());
        assert_eq!(counter.0.load(Ordering::Relaxed), 0);
        cx.set_cancel_requested(true);
        // Check the wake before repolling: a checkpoint-only implementation
        // would strand this operation when the socket/event source is idle.
        assert!(counter.0.load(Ordering::Relaxed) > 0);
        assert_eq!(guarded.as_mut().poll(&mut task), Poll::Ready(Err(SecuredHttpEndpointError::Cancelled)));
        drop(guarded);
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn abandoning_response_guard_without_revalidation_drops_the_operation() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        struct Idle(Arc<AtomicBool>);
        impl Future for Idle {
            type Output = ();
            fn poll(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> Poll<()> { Poll::Pending }
        }
        impl Drop for Idle {
            fn drop(&mut self) { self.0.store(true, Ordering::Release); }
        }
        let cx = Cx::for_testing();
        let dropped = Arc::new(AtomicBool::new(false));
        let mut authorization = None;
        let mut guarded = Box::pin(guard_response(&cx, &mut authorization, Idle(Arc::clone(&dropped))));
        let mut task = std::task::Context::from_waker(std::task::Waker::noop());
        assert!(guarded.as_mut().poll(&mut task).is_pending());
        drop(guarded);
        assert!(dropped.load(Ordering::Acquire));
        assert!(cx.checkpoint().is_ok(), "dropping a response must not cancel its caller");
    }

}
