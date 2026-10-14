//! Caller-owned, origin-guarded native HTTP and HTTPS listeners.
//!
//! Unlike the embedding adapter, this path owns socket admission, bounded H1
//! decoding and response writes. Select it with `Server::bind_secured_http` or
//! `Server::bind_secured_https`. It is modern MCP only, even in a legacy build.
//! Native HTTPS may co-host explicitly configured OAuth routes at its exact
//! public origin. Plain HTTP requires a separate issuer listener.
//!
//! The supplied origin policy is the single CORS authority for this listener.
//! Its exact allowlist also configures the existing downstream request handler.
//! Authentication, authorization and protocol admission are never bypassed.
//! HTTPS performs TLS inside each capacity-admitted connection child; plaintext
//! HTTP remains a separate, explicitly selected listener, never a TLS fallback.

mod connection;
mod ingress;
mod liveness;
mod tls;

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use asupersync::Cx;
use asupersync::io::AsyncWriteExt;
use asupersync::tls::TlsAcceptor;
use fastmcp_core::{CanonicalHttpUrl, McpError, McpResult};
use fastmcp_protocol::protocol_policy::ProtocolPolicy;

use super::super::HttpSecurityPolicy;
use crate::{
    BoundHttpServer, HTTP_ACCEPT_CANCEL_POLL, HttpConnectionChildren,
    HttpListenerShutdown, HttpNonquiescentShutdown, HttpServerShutdown,
    MODERN_HTTP_SESSION_REAP_INTERVAL, Server, detach_live_modern_http_sessions,
    expire_live_modern_http_sessions, finish_live_modern_http_sessions,
    take_unsettled_retired_modern_http_dispatches,
};

/// Finite TLS-handshake, request-read and individual response-write bounds.
/// A handshake is attempted once within its original deadline. Request time is
/// shared by the head and body, not renewed for each trickled byte. Each SSE
/// frame has a finite write allowance; a subscription's execution lifetime is
/// still owned by the server's existing request and response-body policies.
#[derive(Clone, Copy, Debug)]
pub struct SecuredHttpIoLimits {
    handshake_bound: Duration,
    request_timeout: Duration,
    write_timeout: Duration,
}

impl Default for SecuredHttpIoLimits {
    fn default() -> Self {
        Self {
            handshake_bound: Duration::from_secs(10),
            request_timeout: Duration::from_secs(60),
            write_timeout: Duration::from_secs(15),
        }
    }
}

impl SecuredHttpIoLimits {
    pub fn new(request_timeout: Duration, write_timeout: Duration) -> McpResult<Self> {
        if request_timeout.is_zero() || write_timeout.is_zero()
            || request_timeout > Duration::from_mins(15)
            || write_timeout > Duration::from_secs(300)
        {
            return Err(McpError::invalid_request("invalid secured HTTP I/O limits"));
        }
        Ok(Self { handshake_bound: Duration::from_secs(10), request_timeout, write_timeout })
    }

    /// Sets the total TLS handshake allowance, not a timeout per read or retry.
    /// The caller's tighter deadline still wins. Plain HTTP does not use it.
    pub fn with_handshake_timeout(mut self, timeout: Duration) -> McpResult<Self> {
        if timeout.is_zero() || timeout > Duration::from_secs(120) {
            return Err(McpError::invalid_request("invalid secured HTTPS handshake timeout"));
        }
        self.handshake_bound = timeout;
        Ok(self)
    }

    pub fn handshake_timeout(self) -> Duration { self.handshake_bound }
    pub fn request_timeout(self) -> Duration { self.request_timeout }
    pub fn write_timeout(self) -> Duration { self.write_timeout }
}

/// Native bound socket and immutable security/TLS policy. Accepted connection
/// children use the ordinary listener's limiter, response-body registry,
/// termination receipts and caller-owned nonquiescent shutdown outcome.
#[must_use = "serve the bound listener on its caller-owned context"]
pub struct BoundSecuredHttpServer {
    inner: BoundHttpServer,
    policy: Arc<HttpSecurityPolicy>,
    io: SecuredHttpIoLimits,
    tls: Option<TlsAcceptor>,
}

impl Server {
    /// Binds a modern-only secured MCP listener without starting an accept loop.
    ///
    /// The policy path must match the configured MCP path. Selecting this API
    /// explicitly selects its CORS policy: the same exact origins are installed
    /// on the downstream handler, replacing its older CORS allowlist. The lower
    /// of the existing server body limit and policy body limit always wins.
    /// Host validation uses the configured public authority, never Forwarded.
    /// This API is plaintext, for example behind a trusted TLS terminator.
    /// Use `bind_secured_https` for native TLS on the accepted sockets.
    ///
    /// Unsupported protocol policies, route mismatches and co-hosted OAuth routes
    /// fail before a socket is bound or any startup hook runs.
    pub async fn bind_secured_http(
        self,
        cx: &Cx,
        addr: impl Into<String>,
        policy: HttpSecurityPolicy,
    ) -> McpResult<BoundSecuredHttpServer> {
        self.bind_secured_listener(cx, addr, policy, None).await
    }

    async fn bind_secured_listener(
        mut self,
        cx: &Cx,
        addr: impl Into<String>,
        policy: HttpSecurityPolicy,
        tls: Option<TlsAcceptor>,
    ) -> McpResult<BoundSecuredHttpServer> {
        if cx.checkpoint().is_err() { return Err(McpError::request_cancelled()); }
        if self.protocol_policy != ProtocolPolicy::ModernOnly {
            return Err(McpError::invalid_request("secured HTTP requires ModernOnly policy"));
        }
        if self.http_config.handler_config.base_path != policy.endpoint().path() {
            return Err(McpError::invalid_request("secured HTTP policy does not match the MCP route"));
        }
        if let Some(routes) = &self.oauth_http_routes {
            if tls.is_none() {
                return Err(McpError::invalid_request("secured HTTP requires separate OAuth routes; co-hosting requires native HTTPS"));
            }
            let issuer_base = CanonicalHttpUrl::parse(routes.public_endpoint_base())
                .map_err(|_| McpError::invalid_request("invalid secured HTTPS OAuth endpoint"))?;
            if issuer_base.scheme() != policy.public_origin.scheme()
                || issuer_base.host() != policy.public_origin.host()
                || issuer_base.effective_port() != policy.public_origin.effective_port()
            {
                return Err(McpError::invalid_request("secured HTTPS OAuth routes require the same configured public origin"));
            }
            if policy.resource_metadata_path().is_some_and(|path| routes.has_path(path)) {
                return Err(McpError::invalid_request("secured HTTPS OAuth and resource metadata routes overlap"));
            }
            crate::validate_server_http_route_configuration(&self)
                .map_err(|_| McpError::invalid_request("secured HTTPS OAuth routes overlap an installed server route"))?;
        }
        self.http_config.handler_config.allow_cors = true;
        self.http_config.handler_config.cors_origins = policy.origins.clone();
        self.http_config.handler_config.max_body_size = self.http_config.handler_config.max_body_size
            .min(policy.endpoint().limits().max_body_bytes());
        let inner = self.bind_http(cx, addr).await?;
        Ok(BoundSecuredHttpServer {
            inner, policy: Arc::new(policy), io: SecuredHttpIoLimits::default(), tls,
        })
    }

    /// Binds the same secured MCP dispatcher behind native TLS, without an
    /// external reverse proxy or a plaintext forwarding socket.
    ///
    /// Supply an asupersync `TlsAcceptor` built from the deployment's server
    /// identity with `.alpn_protocols(vec![b"http/1.1".to_vec()])`. The actual
    /// configuration must advertise only HTTP/1.1 and disable TLS early data;
    /// either violation fails before binding or running startup hooks. The
    /// acceptor retains responsibility for certificate selection, TLS versions,
    /// optional client-certificate verification and SNI policy. A TLS client
    /// certificate does not replace MCP authentication or grant handler access.
    ///
    /// Handshakes run in capacity-admitted connection children, not in the
    /// accept loop, and use one finite deadline plus the caller's budget. A
    /// stalled client cannot serialize later handshakes. Cancellation, failure
    /// and timeout drop the owned socket; none permits a plaintext retry.
    /// HTTP origin/Host checks, Bearer admission, named scopes, SSE revalidation
    /// and request ownership use the unchanged secured HTTP pipeline.
    ///
    /// Explicit `oauth_http_routes` may share this listener when their public
    /// origin matches the security policy and their paths are disjoint from
    /// MCP and protected-resource metadata. Their existing registration,
    /// consent, PKCE, token and revocation policies remain authoritative; the
    /// listener applies its Host/Origin policy and endpoint-specific byte bounds
    /// before invoking the issuer. Issuer responses are always uncacheable.
    /// The caller must install a blocking pool for synchronous issuer policy;
    /// missing, stopped or saturated pool admission returns HTTP 503 without
    /// executing issuer work on the network executor.
    pub async fn bind_secured_https(
        self,
        cx: &Cx,
        addr: impl Into<String>,
        policy: HttpSecurityPolicy,
        acceptor: TlsAcceptor,
    ) -> McpResult<BoundSecuredHttpServer> {
        if cx.checkpoint().is_err() { return Err(McpError::request_cancelled()); }
        tls::validate_acceptor(&acceptor)?;
        if cx.timer_driver().is_none() {
            return Err(McpError::invalid_request("secured HTTPS requires caller-owned timers"));
        }
        self.bind_secured_listener(cx, addr, policy, Some(acceptor)).await
    }

    /// Binds and serves the secured native listener on the caller's runtime.
    pub async fn serve_secured_http(
        self,
        cx: &Cx,
        addr: impl Into<String>,
        policy: HttpSecurityPolicy,
    ) -> McpResult<HttpServerShutdown> {
        self.bind_secured_http(cx, addr, policy).await?.serve(cx).await
    }

    /// Binds and serves native HTTPS on the caller's runtime and shutdown scope.
    pub async fn serve_secured_https(
        self,
        cx: &Cx,
        addr: impl Into<String>,
        policy: HttpSecurityPolicy,
        acceptor: TlsAcceptor,
    ) -> McpResult<HttpServerShutdown> {
        self.bind_secured_https(cx, addr, policy, acceptor).await?.serve(cx).await
    }
}

impl BoundSecuredHttpServer {
    pub fn local_addr(&self) -> McpResult<SocketAddr> { self.inner.local_addr() }

    /// Whether this bound listener requires TLS on every accepted connection.
    pub fn is_https(&self) -> bool { self.tls.is_some() }

    /// Changes only this not-yet-served listener's finite I/O limits.
    pub fn with_io_limits(mut self, limits: SecuredHttpIoLimits) -> Self {
        self.io = limits;
        self
    }

    /// Accepts secured HTTP requests until caller cancellation or listener error.
    /// Capacity refusal drops the socket without spawning an error-writing task.
    /// TLS handshakes consume that same capacity and remain in the same child
    /// inventory. Shutdown retains the native two-phase terminal drain and
    /// transfers noncooperating children to `HttpServerShutdown::Nonquiescent`.
    ///
    /// No legacy session, runtime, detached task or process-global retention is
    /// created. The listener and all connection children stay in the caller's
    /// region. As with the existing listener, settle a nonquiescent outcome.
    #[allow(clippy::manual_async_fn)]
    pub fn serve(self, cx: &Cx) -> impl Future<Output = McpResult<HttpServerShutdown>> + Send + '_ {
        async move {
            let bound = self.inner;
            let server = Arc::clone(&bound.endpoint.server);
            server.init_rich_logging();
            if let Some(stats) = &server.stats { stats.connection_opened(); }
            if !server.run_startup_hook() {
                server.graceful_shutdown_returning();
                return Err(McpError::internal_error("secured HTTP startup hook failed"));
            }
            let scope = cx.scope();
            let shutdown = HttpListenerShutdown::new(cx);
            let sessions = Arc::clone(&bound.modern_sessions);
            let mut children = HttpConnectionChildren::default();
            let reaper = cx.spawn_in(&scope, move |reaper_cx| async move {
                let chunk = Duration::from_millis(100);
                let mut parked = Duration::ZERO;
                loop {
                    asupersync::time::sleep(reaper_cx.now(), chunk).await;
                    if reaper_cx.checkpoint().is_err() { break; }
                    parked += chunk;
                    if parked >= MODERN_HTTP_SESSION_REAP_INTERVAL {
                        parked = Duration::ZERO;
                        expire_live_modern_http_sessions(&sessions);
                    }
                }
            });
            let mut reaper = match reaper {
                Ok(reaper) => reaper,
                Err(_) => {
                    server.graceful_shutdown_returning();
                    return Err(McpError::internal_error("secured HTTP reaper admission failed"));
                }
            };
            let result = loop {
                children.reap_finished();
                if cx.checkpoint().is_err() { break Ok(()); }
                let accepted = match asupersync::time::timeout(
                    cx.now(), HTTP_ACCEPT_CANCEL_POLL, bound.listener.accept(),
                ).await {
                    Ok(accepted) => accepted,
                    Err(_) => continue,
                };
                let (stream, _) = match accepted {
                    Ok(connection) => connection,
                    Err(_) if cx.checkpoint().is_err() => break Ok(()),
                    Err(_) => break Err(McpError::internal_error("secured HTTP accept failed")),
                };
                let Some(permit) = bound.connection_limiter.try_acquire() else {
                    drop(stream);
                    continue;
                };
                let endpoint = Arc::clone(&bound.endpoint);
                let sessions = Arc::clone(&bound.modern_sessions);
                let policy = Arc::clone(&self.policy);
                let io = self.io;
                let acceptor = self.tls.clone();
                let stopping = shutdown.clone();
                match cx.spawn_in(&scope, move |connection_cx| async move {
                    let _permit = permit;
                    let stream = match acceptor {
                        Some(acceptor) => match tls::accept(
                            &connection_cx, &stopping, stream, &acceptor, io.handshake_bound,
                        ).await {
                            Some(stream) => stream,
                            None => return,
                        },
                        None => tls::ConnectionIo::Plain(stream),
                    };
                    let close = stream.tls_close_handle();
                    let connection: std::pin::Pin<Box<dyn Future<Output = ()> + Send + '_>> = Box::pin(
                        connection::serve(&connection_cx, stream, endpoint, sessions, stopping, policy, io),
                    );
                    // Per-I/O limits cannot bound a silent response wait or
                    // a handler that ignores its deadline. Keep the caller's
                    // cancellation and absolute deadline armed for the whole
                    // owned connection, not just individual writes.
                    let served = liveness::drive(&connection_cx, connection).await;
                    if let Some(mut close) = close
                        && served.is_ok()
                        && connection_cx.checkpoint().is_ok()
                    {
                        // Application response ownership has finished. TLS
                        // close_notify is best-effort: its local timeout must
                        // not extend the caller's remaining connection budget.
                        let _ = liveness::drive(&connection_cx, asupersync::time::timeout(
                            connection_cx.now(), io.write_timeout, close.shutdown(),
                        )).await;
                    }
                }) {
                    Ok(child) => children.tasks.push(child),
                    Err(_) => break Err(McpError::internal_error("secured HTTP connection admission failed")),
                }
            };
            shutdown.request();
            let _ = server.router.close_stateless_mrtr_exchanges();
            let terminal = server.final_subscriptions.terminate_with_receipt();
            reaper.abort();
            let _ = reaper.join(cx).await;
            let closing = detach_live_modern_http_sessions(&bound.modern_sessions);
            children.drain_terminal_controls(&terminal).await;
            children.tasks.extend(finish_live_modern_http_sessions(&bound.modern_sessions, closing).await);
            server.cancel_active_requests(asupersync::types::CancelKind::Shutdown, false);
            let _ = children.drain_cooperative_shutdown().await;
            children.tasks.extend(take_unsettled_retired_modern_http_dispatches(&bound.modern_sessions));
            children.reap_finished();
            server.graceful_shutdown_returning();
            if children.tasks.is_empty() {
                if !children.terminal_failures.is_empty() {
                    return Err(McpError::internal_error("secured HTTP child settlement failed"));
                }
                result?;
                Ok(HttpServerShutdown::Quiescent)
            } else {
                Ok(HttpServerShutdown::Nonquiescent(HttpNonquiescentShutdown {
                    children, listener_error: result.err(),
                }))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secured_io_bounds_are_finite_and_defaults_are_admissible() {
        let limits = SecuredHttpIoLimits::default();
        assert!(SecuredHttpIoLimits::new(limits.request_timeout(), limits.write_timeout()).is_ok());
        assert!(SecuredHttpIoLimits::new(Duration::ZERO, Duration::from_secs(1)).is_err());
        assert!(SecuredHttpIoLimits::new(Duration::from_secs(1), Duration::ZERO).is_err());
        assert!(SecuredHttpIoLimits::new(Duration::from_secs(901), Duration::from_secs(1)).is_err());
        assert!(SecuredHttpIoLimits::new(Duration::from_secs(1), Duration::from_secs(301)).is_err());
        assert_eq!(limits.handshake_timeout(), Duration::from_secs(10));
        assert!(limits.with_handshake_timeout(Duration::from_secs(120)).is_ok());
        assert!(limits.with_handshake_timeout(Duration::ZERO).is_err());
        assert!(limits.with_handshake_timeout(Duration::from_secs(121)).is_err());
    }
}
