//! Who an HTTP or MCP transport request comes from, for the house policy and
//! the action log.
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
//! it grants nothing new; it is honoured only on the loopback listener. A
//! token holder may instead name itself the stdio MCP server
//! ([`LOCAL_CLIENT`]: `mcp-stdio`), as `fsonos mcp` does when it hands its
//! tools to the daemon, so the owner's rules for that client still apply; the
//! same user could run that server directly, so this grants nothing either.
//!
//! On a listener bound to the tailnet (any non-loopback address), a caller
//! is named by the tailnet itself: [`Identity::with_tailnet`] asks a namer
//! (the daemon's is Tailscale's WhoIs) who has the request's peer address,
//! and the answer (a login, `tag:<name>`, or a node name) is that caller's
//! [`Client::Tailnet`]. A peer the tailnet can't name stays `unknown`. A
//! caller from one of the daemon host's own addresses
//! ([`Identity::with_own_addresses`]) is a process on that host, which the
//! tailnet would name as the host's owner: it is `loopback-http`, as it is
//! when it comes over loopback.

use fastapi::Request;
use fastapi::core::middleware::RemoteAddr;
use fsonos_core::policy::Client;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

/// The header Tailscale Serve sets to the requesting user's login name.
pub const SERVE_LOGIN: &str = "tailscale-user-login";

/// Longest login accepted from the header.
const MAX_LOGIN: usize = 256;

/// The header the local CLI proves itself with; see the module docs.
pub const CLI_TOKEN: &str = "x-fsonos-cli-token";

/// The header with which a caller holding the CLI token names itself the
/// stdio MCP server (`mcp-stdio`) rather than the CLI.
pub const LOCAL_CLIENT: &str = "x-fsonos-client";

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
    own: Vec<IpAddr>,
}

// By hand, so the CLI token never reaches a log.
impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("listener", &self.listener)
            .field("serve_headers", &self.serve_headers)
            .field("cli_token", &self.cli_token.as_ref().map(|_| "<set>"))
            .field("tailnet", &self.tailnet.is_some())
            .field("own", &self.own)
            .finish()
    }
}

// By hand: namers compare as the same one.
impl PartialEq for Identity {
    fn eq(&self, other: &Self) -> bool {
        self.listener == other.listener
            && self.serve_headers == other.serve_headers
            && self.cli_token == other.cli_token
            && self.own == other.own
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
            own: Vec::new(),
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
            own: Vec::new(),
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

    /// On an `unknown` listener, name a caller from one of `addrs` (the
    /// daemon host's own addresses) `loopback-http`; see the module docs.
    #[must_use]
    pub fn with_own_addresses(mut self, addrs: impl IntoIterator<Item = IpAddr>) -> Self {
        self.own = addrs.into_iter().collect();
        self
    }

    /// The caller of `req`.
    #[must_use]
    pub fn of(&self, req: &Request) -> Client {
        let peer = req.get_extension::<RemoteAddr>().map(|RemoteAddr(ip)| *ip);
        self.resolve(peer, |name| req.headers().get(name), true)
    }

    /// The caller of a transport request, from its actual socket peer and
    /// parsed HTTP headers. Forwarded addresses are never used. This is the
    /// MCP listener's identity path: local CLI credentials are never accepted,
    /// even if this identity has a configured CLI token. Only [`Self::of`]
    /// admits those credentials on the daemon's HTTP API.
    #[must_use]
    pub fn of_http_parts(&self, peer: SocketAddr, headers: &[(String, String)]) -> Client {
        self.resolve(
            Some(peer.ip()),
            |name| {
                let mut values = headers
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case(name));
                let (_, value) = values.next()?;
                // Ambiguous identity headers cannot name a caller.
                values.next().is_none().then_some(value.as_bytes())
            },
            false,
        )
    }

    fn resolve<'a>(
        &self,
        peer: Option<IpAddr>,
        header: impl Fn(&str) -> Option<&'a [u8]>,
        local_credentials: bool,
    ) -> Client {
        if self.listener == Client::Unknown
            && let Some(peer) = peer
            && self.own.contains(&peer)
        {
            return Client::LoopbackHttp;
        }
        if self.listener == Client::Unknown
            && let Some(namer) = &self.tailnet
            && let Some(peer) = peer
            && let Some(name) = namer(peer).filter(|n| plausible(n))
        {
            return Client::Tailnet(name);
        }
        if local_credentials
            && self.listener == Client::LoopbackHttp
            && let Some(token) = &self.cli_token
            && header(CLI_TOKEN).is_some_and(|sent| same_secret(sent, token.as_bytes()))
        {
            let stdio = header(LOCAL_CLIENT)
                .and_then(|v| std::str::from_utf8(v).ok())
                .is_some_and(|v| v.trim() == "mcp-stdio");
            return if stdio { Client::McpStdio } else { Client::Cli };
        }
        if !self.serve_headers || self.listener != Client::LoopbackHttp {
            return self.listener.clone();
        }
        header(SERVE_LOGIN)
            .and_then(|v| std::str::from_utf8(v).ok())
            .map(str::trim)
            .filter(|login| plausible(login))
            .map_or_else(
                || self.listener.clone(),
                |login| Client::Tailnet(login.to_string()),
            )
    }
}

/// The keys the daemon's own clients go by ([`Client::key`]): a tailnet
/// name that took one would get that client's rules, and a schedule it
/// made would run as that client.
const RESERVED: [&str; 4] = ["cli", "mcp-stdio", "loopback-http", "unknown"];

/// A name a caller can go by: not empty, bounded, no control characters,
/// and none of [`RESERVED`].
fn plausible(name: &str) -> bool {
    !name.trim().is_empty()
        && name.len() <= MAX_LOGIN
        && !name.chars().any(char::is_control)
        && !RESERVED
            .iter()
            .any(|key| key.eq_ignore_ascii_case(name.trim()))
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
        // A daemon client's own key is no tailnet name.
        for reserved in ["cli", "CLI", "mcp-stdio", "loopback-http", "unknown"] {
            assert_eq!(
                serve.of(&request(Some(reserved))),
                Client::LoopbackHttp,
                "{reserved}"
            );
        }
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

    #[test]
    fn a_caller_from_the_hosts_own_address_is_a_local_process_not_its_owner() {
        let from = |ip: &str| {
            let mut req = request(None);
            req.insert_extension(RemoteAddr(ip.parse().unwrap()));
            req
        };
        // The tailnet names the host's own address as its owner.
        let namer = |peer: IpAddr| match peer.to_string().as_str() {
            "100.64.0.5" | "fd7a:115c:a1e0::5" | "100.64.0.7" => {
                Some("owner@example.com".to_owned())
            }
            _ => None,
        };
        let own: [IpAddr; 2] = [
            "100.64.0.5".parse().unwrap(),
            "fd7a:115c:a1e0::5".parse().unwrap(),
        ];
        let tailnet = Identity::fixed(Client::Unknown)
            .with_tailnet(namer)
            .with_own_addresses(own);
        assert_eq!(tailnet.of(&from("100.64.0.5")), Client::LoopbackHttp);
        assert_eq!(tailnet.of(&from("fd7a:115c:a1e0::5")), Client::LoopbackHttp);
        // The owner's other devices are still the owner.
        assert_eq!(
            tailnet.of(&from("100.64.0.7")),
            Client::Tailnet("owner@example.com".into())
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

    #[test]
    fn a_token_holder_may_name_itself_the_stdio_mcp_server_and_nothing_else() {
        let token = "0123456789abcdef0123456789abcdef";
        let loopback = Identity::behind_serve(Client::LoopbackHttp).with_cli_token(token.into());
        let claiming = |token: &str, client: &str| {
            let mut req = with_token(token);
            req.headers_mut()
                .insert(LOCAL_CLIENT, client.as_bytes().to_vec());
            req
        };
        assert_eq!(loopback.of(&claiming(token, "mcp-stdio")), Client::McpStdio);
        // Any other name is the CLI, as without the header.
        assert_eq!(loopback.of(&claiming(token, "tailnet")), Client::Cli);
        // Without the token the claim means nothing: the ordinary caller.
        assert_eq!(
            loopback.of(&claiming("wrong", "mcp-stdio")),
            Client::Tailnet("ada@example.com".into())
        );
        let tailnet = Identity::fixed(Client::Unknown).with_cli_token(token.into());
        assert_eq!(tailnet.of(&claiming(token, "mcp-stdio")), Client::Unknown);
    }

    fn headers(values: &[(&str, &str)]) -> Vec<(String, String)> {
        values
            .iter()
            .map(|(name, value)| ((*name).into(), (*value).into()))
            .collect()
    }

    #[test]
    fn transport_parts_and_http_requests_share_real_peer_resolution() {
        let own: [IpAddr; 2] = [
            "100.64.0.5".parse().unwrap(),
            "fd7a:115c:a1e0::5".parse().unwrap(),
        ];
        let identity = Identity::behind_serve(Client::Unknown)
            .with_tailnet(|peer| match peer.to_string().as_str() {
                "100.64.0.7" => Some("grace@example.com".into()),
                "fd7a:115c:a1e0::7" => Some("tag:agent".into()),
                "100.64.0.5" | "fd7a:115c:a1e0::5" => {
                    panic!("the host's own address must not be named as its owner")
                }
                _ => None,
            })
            .with_own_addresses(own);
        let headers = headers(&[
            ("Tailscale-User-Login", "forged@example.com"),
            ("Forwarded", "for=100.64.0.7"),
            ("X-Forwarded-For", "100.64.0.7"),
            ("X-Real-IP", "100.64.0.7"),
            (LOCAL_CLIENT, "cli"),
        ]);
        for (peer, expected) in [
            (
                "100.64.0.7:40000",
                Client::Tailnet("grace@example.com".into()),
            ),
            (
                "[fd7a:115c:a1e0::7]:40000",
                Client::Tailnet("tag:agent".into()),
            ),
            ("100.64.0.5:40000", Client::LoopbackHttp),
            ("[fd7a:115c:a1e0::5]:40000", Client::LoopbackHttp),
            ("100.64.0.8:40000", Client::Unknown),
        ] {
            let peer: SocketAddr = peer.parse().unwrap();
            let mut req = request(None);
            req.insert_extension(RemoteAddr(peer.ip()));
            for (name, value) in &headers {
                req.headers_mut().insert(name, value.as_bytes().to_vec());
            }
            assert_eq!(identity.of_http_parts(peer, &headers), expected, "{peer}");
            assert_eq!(identity.of(&req), expected, "HTTP peer {peer}");
        }
    }

    #[test]
    fn transport_serve_identity_requires_opt_in_and_a_loopback_listener() {
        let headers = headers(&[("TaIlScAlE-UsEr-LoGiN", " ada@example.com ")]);
        for peer in ["127.0.0.1:40000", "[::1]:40000"] {
            let peer = peer.parse().unwrap();
            assert_eq!(
                Identity::behind_serve(Client::LoopbackHttp).of_http_parts(peer, &headers),
                Client::Tailnet("ada@example.com".into())
            );
            assert_eq!(
                Identity::fixed(Client::LoopbackHttp).of_http_parts(peer, &headers),
                Client::LoopbackHttp
            );
        }
        assert_eq!(
            Identity::behind_serve(Client::Unknown)
                .of_http_parts("100.64.0.7:40000".parse().unwrap(), &headers),
            Client::Unknown
        );
        assert_eq!(
            Identity::behind_serve(Client::LoopbackHttp)
                .of_http_parts("127.0.0.1:40000".parse().unwrap(), &[]),
            Client::LoopbackHttp,
            "tagged Serve devices without login headers remain local callers"
        );
    }

    #[test]
    fn transport_identity_rejects_reserved_malformed_and_duplicate_names() {
        let loopback = "127.0.0.1:40000".parse().unwrap();
        let tailnet = "100.64.0.7:40000".parse().unwrap();
        let serve = Identity::behind_serve(Client::LoopbackHttp);
        for name in [
            "",
            "  ",
            "cli",
            " CLI ",
            "mcp-stdio",
            "loopback-http",
            "unknown",
            "bad\nname",
        ] {
            assert_eq!(
                serve.of_http_parts(loopback, &headers(&[(SERVE_LOGIN, name)])),
                Client::LoopbackHttp,
                "Serve {name:?}"
            );
            let name = name.to_string();
            let whois = Identity::fixed(Client::Unknown).with_tailnet(move |_| Some(name.clone()));
            assert_eq!(whois.of_http_parts(tailnet, &[]), Client::Unknown);
        }
        let too_long = "a".repeat(MAX_LOGIN + 1);
        assert_eq!(
            serve.of_http_parts(loopback, &headers(&[(SERVE_LOGIN, &too_long)])),
            Client::LoopbackHttp
        );
        assert_eq!(
            serve.of_http_parts(
                loopback,
                &headers(&[
                    ("Tailscale-User-Login", "ada@example.com"),
                    (SERVE_LOGIN, "grace@example.com"),
                ]),
            ),
            Client::LoopbackHttp,
            "a parser-dependent duplicate header must not choose the principal"
        );
    }

    #[test]
    fn transport_never_accepts_cli_or_stdio_credentials() {
        let peer = "127.0.0.1:40000".parse().unwrap();
        let token = "0123456789abcdef0123456789abcdef";
        let identity = Identity::behind_serve(Client::LoopbackHttp).with_cli_token(token.into());
        for client in ["cli", "mcp-stdio", "owner@example.com"] {
            let mut sent = headers(&[(CLI_TOKEN, token), (LOCAL_CLIENT, client)]);
            assert_eq!(identity.of_http_parts(peer, &sent), Client::LoopbackHttp);
            sent.push((SERVE_LOGIN.into(), "ada@example.com".into()));
            assert_eq!(
                identity.of_http_parts(peer, &sent),
                Client::Tailnet("ada@example.com".into())
            );
        }
        // The API's existing local CLI and stdio behavior remains available.
        assert_eq!(identity.of(&with_token(token)), Client::Cli);
        let mut stdio = with_token(token);
        stdio.headers_mut().insert(LOCAL_CLIENT, b"mcp-stdio".to_vec());
        assert_eq!(identity.of(&stdio), Client::McpStdio);
    }
}
