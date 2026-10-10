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
//!
//! On a listener bound to the tailnet (any non-loopback address), a caller
//! is named by the tailnet itself: [`Identity::with_tailnet`] asks a namer
//! (the daemon's is Tailscale's WhoIs) who has the request's peer address,
//! and the answer (a login, `tag:<name>`, or a node name) is that caller's
//! [`Client::Tailnet`]. A peer the tailnet can't name stays `unknown`.

use fastapi::Request;
use fastapi::core::middleware::RemoteAddr;
use fsonos_core::policy::Client;
use std::net::IpAddr;
use std::sync::Arc;

/// The header Tailscale Serve sets to the requesting user's login name.
pub const SERVE_LOGIN: &str = "tailscale-user-login";

/// Longest login accepted from the header.
const MAX_LOGIN: usize = 256;

/// The header the local CLI proves itself with; see the module docs.
pub const CLI_TOKEN: &str = "x-fsonos-cli-token";

/// Names a tailnet peer by its address (a login, `tag:<name>`, or a node
/// name), or `None` when the tailnet can't say who it is.
pub type TailnetNamer = Arc<dyn Fn(IpAddr) -> Option<String> + Send + Sync>;

/// How a listener identifies its callers; see the module docs.
#[derive(Clone)]
pub struct Identity {
    listener: Client,
    serve_headers: bool,
    cli_token: Option<String>,
    tailnet: Option<TailnetNamer>,
}

// By hand, so the CLI token never reaches a log.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("listener", &self.listener)
            .field("serve_headers", &self.serve_headers)
            .field("cli_token", &self.cli_token.as_ref().map(|_| "<set>"))
            .field("tailnet", &self.tailnet.is_some())
            .finish()
    }
}

// By hand: namers compare as the same one.
impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        self.listener == other.listener
            && self.serve_headers == other.serve_headers
            && self.cli_token == other.cli_token
            && match (&self.tailnet, &other.tailnet) {
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                (a, b) => a.is_none() && b.is_none(),
            }
    }
}

impl Eq for Identity {}

impl Identity {
    /// Every caller is `client`.
    #[must_use]
    pub fn fixed(client: Client) -> Self {
        Self {
            listener: client,
            serve_headers: false,
            cli_token: None,
            tailnet: None,
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
            tailnet: None,
        }
    }

    /// Also name a loopback caller presenting `token` in [`CLI_TOKEN`] the
    /// CLI; see the module docs. An empty token is never accepted.
    #[must_use]
    pub fn with_cli_token(mut self, token: String) -> Self {
        self.cli_token = Some(token).filter(|t| !t.is_empty());
        self
    }

    /// Name an `unknown` listener's callers by `namer` (see the module
    /// docs); loopback listeners are unchanged.
    #[must_use]
    pub fn with_tailnet(
        mut self,
        namer: impl Fn(IpAddr) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.tailnet = Some(Arc::new(namer));
        self
    }

    /// The caller of `req`.
    #[must_use]
    pub fn of(&self, req: &Request) -> Client {
        if self.listener == Client::Unknown
            && let Some(namer) = &self.tailnet
            && let Some(RemoteAddr(peer)) = req.get_extension::<RemoteAddr>()
            && let Some(name) = namer(*peer).filter(|n| plausible(n))
        {
            return Client::Tailnet(name);
        }
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
            .filter(|login| plausible(login))
            .map_or_else(
                || self.listener.clone(),
                |login| Client::Tailnet(login.to_string()),
            )
    }
}

/// A name a caller can go by: not empty, bounded, no control characters.
fn plausible(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_LOGIN && !name.chars().any(char::is_control)
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

    #[test]
    fn a_tailnet_listener_names_its_callers_by_the_tailnet() {
        let from = |ip: &str| {
            let mut req = request(Some("ada@example.com"));
            req.insert_extension(RemoteAddr(ip.parse().unwrap()));
            req
        };
        let namer = |peer: IpAddr| match peer.to_string().as_str() {
            "100.64.0.7" => Some("grace@example.com".to_owned()),
            "100.64.0.8" => Some("tag:agent".to_owned()),
            "100.64.0.9" => Some("bad\u{7}name".to_owned()),
            _ => None,
        };
        let direct = Identity::fixed(Client::Unknown).with_tailnet(namer);
        assert_eq!(
            direct.of(&from("100.64.0.7")),
            Client::Tailnet("grace@example.com".into()),
            "the tailnet's name, not a header's"
        );
        assert_eq!(
            direct.of(&from("100.64.0.8")),
            Client::Tailnet("tag:agent".into())
        );
        // Unnamed, implausibly named, or no address known: still unknown.
        assert_eq!(direct.of(&from("100.64.0.1")), Client::Unknown);
        assert_eq!(direct.of(&from("100.64.0.9")), Client::Unknown);
        assert_eq!(direct.of(&request(None)), Client::Unknown);
        // Loopback listeners are not the tailnet's to name.
        let loopback = Identity::behind_serve(Client::LoopbackHttp).with_tailnet(namer);
        assert_eq!(
            loopback.of(&from("100.64.0.7")),
            Client::Tailnet("ada@example.com".into()),
            "Serve's login, as before"
        );
        assert_eq!(
            Identity::fixed(Client::LoopbackHttp)
                .with_tailnet(namer)
                .of(&from("100.64.0.7")),
            Client::LoopbackHttp
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
