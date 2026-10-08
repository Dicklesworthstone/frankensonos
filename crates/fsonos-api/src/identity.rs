//! Who an HTTP request comes from, for the house policy and the action log.
//!
//! A listener's callers are its [`Client`]: `loopback-http` on loopback (local
//! processes, and Tailscale Serve, which forwards to loopback), `unknown`
//! anywhere else. When the daemon runs behind Serve
//! ([`Identity::behind_serve`], `fsonos serve --tailscale-serve`), a loopback
//! request carrying Serve's `Tailscale-User-Login` header is that tailnet
//! user instead, so `[clients."<login>"]` in `policy.toml` applies to them and
//! the log names them. Serve sends the header only for user-owned devices;
//! requests from tagged devices stay `loopback-http`.
//!
//! The header is trusted because only Serve is expected to reach the
//! loopback listener with it; a local process on the daemon host could send
//! it as well, so a login should not be granted more than the host's own
//! processes are trusted with.

use fastapi::Request;
use fsonos_core::policy::Client;

/// The header Tailscale Serve sets to the requesting user's login name.
pub const SERVE_LOGIN: &str = "tailscale-user-login";

/// Longest login accepted from the header.
const MAX_LOGIN: usize = 256;

/// How a listener identifies its callers; see the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    listener: Client,
    serve_headers: bool,
}

impl Identity {
    /// Every caller is `client`.
    #[must_use]
    pub fn fixed(client: Client) -> Self {
        Self {
            listener: client,
            serve_headers: false,
        }
    }

    /// Callers are `client`, except that on a loopback listener Serve's
    /// login header names the tailnet user.
    #[must_use]
    pub fn behind_serve(client: Client) -> Self {
        Self {
            listener: client,
            serve_headers: true,
        }
    }

    /// The caller of `req`.
    #[must_use]
    pub fn of(&self, req: &Request) -> Client {
        if !self.serve_headers || self.listener != Client::LoopbackHttp {
            return self.listener.clone();
        }
        req.headers()
            .get(SERVE_LOGIN)
            .and_then(|v| std::str::from_utf8(v).ok())
            .map(str::trim)
            .filter(|login| {
                !login.is_empty()
                    && login.len() <= MAX_LOGIN
                    && !login.chars().any(char::is_control)
            })
            .map_or_else(
                || self.listener.clone(),
                |login| Client::Tailnet(login.to_string()),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi::Method;

    fn request(login: Option<&str>) -> Request {
        let mut req = Request::new(Method::Post, "/pause");
        if let Some(login) = login {
            req.headers_mut()
                .insert("Tailscale-User-Login", login.as_bytes().to_vec());
        }
        req
    }

    #[test]
    fn serves_login_names_the_caller_only_behind_serve_on_loopback() {
        let serve = Identity::behind_serve(Client::LoopbackHttp);
        assert_eq!(
            serve.of(&request(Some("ada@example.com"))),
            Client::Tailnet("ada@example.com".into())
        );
        assert_eq!(serve.of(&request(None)), Client::LoopbackHttp);
        assert_eq!(serve.of(&request(Some("  "))), Client::LoopbackHttp);
        assert_eq!(serve.of(&request(Some("a\u{7}b"))), Client::LoopbackHttp);
        // Not configured for Serve: the header means nothing.
        let plain = Identity::fixed(Client::LoopbackHttp);
        assert_eq!(
            plain.of(&request(Some("ada@example.com"))),
            Client::LoopbackHttp
        );
        // A tailnet-bound listener is not Serve's: its callers stay unknown.
        let direct = Identity::behind_serve(Client::Unknown);
        assert_eq!(
            direct.of(&request(Some("ada@example.com"))),
            Client::Unknown
        );
    }
}
