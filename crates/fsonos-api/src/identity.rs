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
//!
//! The CLI goes through the daemon as itself ([`Client::Cli`]) by sending
//! the token `fsonos serve` wrote to its data directory
//! ([`Identity::with_cli_token`], header [`CLI_TOKEN`]). Reading the token
//! takes what running the CLI directly takes (the daemon user's files), so
//! it grants nothing new; it is honoured only on the loopback listener.

use fastapi::Request;
use fsonos_core::policy::Client;

/// The header Tailscale Serve sets to the requesting user's login name.
pub const SERVE_LOGIN: &str = "tailscale-user-login";

/// Longest login accepted from the header.
const MAX_LOGIN: usize = 256;

/// The header the local CLI proves itself with; see the module docs.
pub const CLI_TOKEN: &str = "x-fsonos-cli-token";

/// How a listener identifies its callers; see the module docs.
#[derive(Clone, PartialEq, Eq)]
pub struct Identity {
    listener: Client,
    serve_headers: bool,
    cli_token: Option<String>,
}

// By hand, so the CLI token never reaches a log.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("listener", &self.listener)
            .field("serve_headers", &self.serve_headers)
            .field("cli_token", &self.cli_token.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl Identity {
    /// Every caller is `client`.
    #[must_use]
    pub fn fixed(client: Client) -> Self {
        Self {
            listener: client,
            serve_headers: false,
            cli_token: None,
        }
    }

    /// Callers are `client`, except that on a loopback listener Serve's
    /// login header names the tailnet user.
    #[must_use]
    pub fn behind_serve(client: Client) -> Self {
        Self {
            listener: client,
            serve_headers: true,
            cli_token: None,
        }
    }

    /// Also name a loopback caller presenting `token` in [`CLI_TOKEN`] the
    /// CLI; see the module docs. An empty token is never accepted.
    #[must_use]
    pub fn with_cli_token(mut self, token: String) -> Self {
        self.cli_token = Some(token).filter(|t| !t.is_empty());
        self
    }

    /// The caller of `req`.
    #[must_use]
    pub fn of(&self, req: &Request) -> Client {
        if self.listener == Client::LoopbackHttp
            && let Some(token) = &self.cli_token
            && req
                .headers()
                .get(CLI_TOKEN)
                .is_some_and(|sent| same_secret(sent, token.as_bytes()))
        {
            return Client::Cli;
        }
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

/// `a == b`, taking the same time wherever they first differ.
fn same_secret(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
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

    fn with_token(token: &str) -> Request {
        let mut req = request(Some("ada@example.com"));
        req.headers_mut()
            .insert(CLI_TOKEN, token.as_bytes().to_vec());
        req
    }

    #[test]
    fn the_cli_token_names_the_cli_only_on_loopback_and_only_when_it_matches() {
        let token = "0123456789abcdef0123456789abcdef";
        let loopback = Identity::behind_serve(Client::LoopbackHttp).with_cli_token(token.into());
        assert_eq!(loopback.of(&with_token(token)), Client::Cli);
        // A wrong or partial token is the ordinary caller (here Serve's login).
        for wrong in ["0123456789abcdef0123456789abcdee", "0123", ""] {
            assert_eq!(
                loopback.of(&with_token(wrong)),
                Client::Tailnet("ada@example.com".into()),
                "{wrong:?}"
            );
        }
        // Without a configured token, or off loopback, the header means nothing.
        let none = Identity::fixed(Client::LoopbackHttp);
        assert_eq!(none.of(&with_token(token)), Client::LoopbackHttp);
        let tailnet = Identity::fixed(Client::Unknown).with_cli_token(token.into());
        assert_eq!(tailnet.of(&with_token(token)), Client::Unknown);
        let empty = Identity::fixed(Client::LoopbackHttp).with_cli_token(String::new());
        assert_eq!(empty.of(&with_token("")), Client::LoopbackHttp);
    }
}
