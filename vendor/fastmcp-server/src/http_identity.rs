//! Trusted identity supplied by a native HTTP listener's embedding application.
//!
//! Raw network metadata stays at admission. Dispatch receives the same bounded
//! authentication receipt and session-owner fingerprint as bearer admission.

use std::net::SocketAddr;
use std::sync::Arc;

use fastmcp_core::{AuthContext, McpResult};
use fastmcp_transport::http::{HttpRequest, HttpResponse, HttpStatus};

use crate::router::TransportAuthorization;
use crate::{Server, auth, catch_extension_unwind};

type Resolve = dyn Fn(SocketAddr, &[(String, String)]) -> McpResult<AuthContext> + Send + Sync;

/// A listener-owned resolver for authenticated native HTTP identity.
///
/// The callback sees the actual accepted socket peer and validated, normalized
/// HTTP headers, never identity claims taken from JSON-RPC arguments. A trusted
/// proxy deployment must explicitly decide which peers may supply its identity
/// headers. Return an anonymous or named read-only identity when appropriate;
/// an error rejects the request without falling back to another identity.
///
/// This synchronous callback must return promptly and bound any lookup it makes.
/// Neither the callback nor its captured configuration is rendered by `Debug`.
#[derive(Clone)]
pub struct HttpIdentityResolver(Arc<Resolve>);

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
        Self(Arc::new(resolve))
    }
}

impl Server {
    /// Native H1 alone can attach this custody. Public in-process HTTP requests
    /// have no accepted socket peer and cannot manufacture these facts.
    pub(crate) fn resolve_native_http_identity(
        &self,
        peer: SocketAddr,
        request: &HttpRequest,
        authorization: TransportAuthorization,
    ) -> Result<TransportAuthorization, HttpResponse> {
        let Some(resolver) = &self.http_config.identity_resolver else {
            return Ok(authorization);
        };
        // A listener chooses one authentication boundary. A peer cannot select
        // an alternate bearer path or smuggle a token to the trusted resolver.
        if self.auth_provider.is_some() || authorization.has_header() {
            return Err(rejected());
        }
        let headers = request
            .headers
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect::<Vec<_>>();
        let facts = catch_extension_unwind(|| (resolver.0)(peer, &headers))
            .map_err(|_| rejected())?
            .map_err(|_| rejected())?;
        auth::principal_fingerprint(Some(&facts)).map_err(|_| rejected())?;
        Ok(authorization.with_identity(facts))
    }
}

pub(crate) fn rejected() -> HttpResponse {
    // A tailnet/proxy identity failure is not a Bearer authentication challenge.
    HttpResponse::new(HttpStatus::FORBIDDEN)
}

#[cfg(test)]
mod tests;
