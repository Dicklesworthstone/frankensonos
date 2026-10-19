//! Trusted identity supplied by a native HTTP listener's embedding application.
//!
//! Raw network metadata stays at admission. Dispatch receives the same bounded
//! authentication receipt and session-owner fingerprint as bearer admission.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use asupersync::Cx;
use fastmcp_core::{AuthContext, McpResult};
use fastmcp_transport::http::{HttpRequest, HttpResponse, HttpStatus};

use crate::router::TransportAuthorization;
use crate::{
    BlockingTaskGuard, Server, auth, blocking_dispatch_pool, catch_extension_unwind,
    try_reserve_blocking_dispatch,
};

type Resolve = dyn Fn(SocketAddr, &[(String, String)]) -> McpResult<AuthContext> + Send + Sync;

/// Running or queued identity callbacks owned by one listener configuration.
const MAX_IDENTITY_JOBS: usize = 8;

/// A listener-owned resolver for authenticated native HTTP identity.
///
/// The callback sees the actual accepted socket peer and validated, normalized
/// HTTP headers, never identity claims taken from JSON-RPC arguments. A trusted
/// proxy deployment must explicitly decide which peers may supply its identity
/// headers. Return an anonymous or named read-only identity when appropriate;
/// an error rejects the request without falling back to another identity.
///
/// Native listeners run this synchronous callback on a bounded blocking pool.
/// It must still bound its lookup: cancellation cannot interrupt synchronous
/// application code, and its admission slot stays occupied until it returns.
/// Neither the callback nor its captured configuration is rendered by `Debug`.
#[derive(Clone)]
pub struct HttpIdentityResolver {
    resolve: Arc<Resolve>,
    pending: Arc<AtomicUsize>,
}

impl std::fmt::Debug for HttpIdentityResolver {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HttpIdentityResolver { .. }")
    }
}

impl HttpIdentityResolver {
    /// Install an embedding-owned resolver; see the type's trust contract.
    #[must_use]
    pub fn new(
        resolve: impl Fn(SocketAddr, &[(String, String)]) -> McpResult<AuthContext>
        + Send
        + Sync
        + 'static,
    ) -> Self {
        Self {
            resolve: Arc::new(resolve),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn reserve(&self) -> Option<IdentityPermit> {
        self.pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < MAX_IDENTITY_JOBS).then_some(pending + 1)
            })
            .ok()
            .map(|_| IdentityPermit(Arc::clone(&self.pending)))
    }

    fn resolve(
        &self,
        peer: SocketAddr,
        headers: &[(String, String)],
    ) -> Result<AuthContext, HttpResponse> {
        let facts = catch_extension_unwind(|| (self.resolve)(peer, headers))
            .map_err(|_| rejected())?
            .map_err(|_| rejected())?;
        auth::principal_fingerprint(Some(&facts)).map_err(|_| rejected())?;
        Ok(facts)
    }
}

struct IdentityPermit(Arc<AtomicUsize>);

impl Drop for IdentityPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl Server {
    fn native_http_resolver(
        &self,
        authorization: &TransportAuthorization,
    ) -> Result<Option<HttpIdentityResolver>, HttpResponse> {
        let Some(resolver) = &self.http_config.identity_resolver else {
            return Ok(None);
        };
        // A listener chooses one authentication boundary. A peer cannot select
        // an alternate bearer path or smuggle a token to the trusted resolver.
        if self.auth_provider.is_some() || authorization.has_header() {
            return Err(rejected());
        }
        Ok(Some(resolver.clone()))
    }

    /// Native H1 alone can attach this custody. Public in-process HTTP requests
    /// have no accepted socket peer and cannot manufacture these facts.
    #[cfg(test)]
    pub(crate) fn resolve_native_http_identity(
        &self,
        peer: SocketAddr,
        request: &HttpRequest,
        authorization: TransportAuthorization,
    ) -> Result<TransportAuthorization, HttpResponse> {
        let Some(resolver) = self.native_http_resolver(&authorization)? else {
            return Ok(authorization);
        };
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        resolver
            .resolve(peer, &headers)
            .map(|facts| authorization.with_identity(facts))
    }

    /// Keep potentially blocking identity lookups off native I/O workers.
    ///
    /// The child remains owned by the connection's region until the real pool
    /// task terminates, including during cancellation. A dropped waiter cannot
    /// release its admission permit while synchronous application code runs.
    pub(crate) async fn resolve_native_http_identity_async(
        &self,
        cx: &Cx,
        peer: SocketAddr,
        request: &HttpRequest,
        authorization: TransportAuthorization,
    ) -> Result<TransportAuthorization, HttpResponse> {
        let Some(resolver) = self.native_http_resolver(&authorization)? else {
            return Ok(authorization);
        };
        cx.checkpoint().map_err(|_| unavailable())?;
        let identity_permit = resolver.reserve().ok_or_else(unavailable)?;
        let dispatch_permit = try_reserve_blocking_dispatch().ok_or_else(unavailable)?;
        // The request body stays with the I/O task. Only bounded header facts
        // cross to the resolver, which cannot observe or edit the MCP payload.
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let mut dispatch = cx
            .spawn(move |dispatch_cx| async move {
                let (sender, mut receiver) = asupersync::channel::oneshot::channel();
                let task = blocking_dispatch_pool().spawn(move || {
                    let _identity_permit = identity_permit;
                    let _dispatch_permit = dispatch_permit;
                    let _lane = fastmcp_core::runtime::enter_blocking_lane();
                    let answer = resolver.resolve(peer, &headers);
                    let _ = sender.send_blocking(answer);
                });
                let task = BlockingTaskGuard(task);
                // Match the native legacy dispatch bridge's completion fence.
                // Claimed blocking work cannot be force-stopped or detached from
                // the region that must report a nonquiescent shutdown.
                while !task.0.is_done() {
                    asupersync::time::sleep(dispatch_cx.now(), Duration::from_millis(1)).await;
                    if dispatch_cx.is_cancel_requested() {
                        asupersync::runtime::yield_now().await;
                    }
                }
                let answer = receiver.try_recv().map_err(|_| unavailable());
                drop(task);
                answer?
            })
            .map_err(|_| unavailable())?;
        let answer = dispatch.join(cx).await.map_err(|_| unavailable())?;
        cx.checkpoint().map_err(|_| unavailable())?;
        answer.map(|facts| authorization.with_identity(facts))
    }
}

fn unavailable() -> HttpResponse {
    HttpResponse::new(HttpStatus::SERVICE_UNAVAILABLE)
}

pub(crate) fn rejected() -> HttpResponse {
    // A tailnet/proxy identity failure is not a Bearer authentication challenge.
    HttpResponse::new(HttpStatus::FORBIDDEN)
}

#[cfg(test)]
mod tests;
