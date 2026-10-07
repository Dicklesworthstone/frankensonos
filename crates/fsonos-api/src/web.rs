//! Browser safety for the HTTP listeners.
//!
//! The control API has no authentication of its own: reachability is
//! control. So a web page open in a browser on the daemon host, or on any
//! tailnet device, must not be able to drive the speakers or read the house
//! through it. A [`WebPolicy`] holds the rules for one listener:
//!
//! * **Host**: only the listener's own names (loopback names and addresses,
//!   the bound address, the tailnet's MagicDNS name and addresses). The
//!   listener's `ServerConfig::with_allowed_hosts` enforces this
//!   ([`WebPolicy::hosts`]), which defeats DNS rebinding.
//! * **Origin**: a request that carries one must come from one of the
//!   daemon's own origins, else `403 UNTRUSTED_ORIGIN`. CLI tools and agents
//!   send no Origin and are unaffected.
//! * **Writes are JSON**: every POST must be `application/json`, else
//!   `415 UNSUPPORTED_MEDIA_TYPE`. That closes the cross-origin POST a browser
//!   sends without a CORS preflight (`text/plain`, forms).
//!
//! No `Access-Control-Allow-Origin` header is ever sent.

use fastapi::Request;
use std::net::SocketAddr;

use crate::failure::{ErrorCode, Failure};

/// The browser-safety rules for one listener. See the module docs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebPolicy {
    hosts: Vec<String>,
    origins: Vec<String>,
}

fn host_literal(host: &str) -> String {
    if host.parse::<std::net::Ipv6Addr>().is_ok() {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

impl WebPolicy {
    /// The rules for a listener bound to `addr`, also answering for `names`
    /// (the tailnet's MagicDNS name and addresses, say, which Tailscale Serve
    /// or tailnet clients use).
    #[must_use]
    pub fn for_listener(addr: SocketAddr, names: &[String]) -> Self {
        let mut hosts: Vec<String> = Vec::new();
        let mut add = |h: String| {
            // An IPv6 literal goes in brackets, the form a Host header (and
            // so fastapi's Host pattern) gives it.
            let h = host_literal(h.trim().trim_end_matches('.')).to_ascii_lowercase();
            if !h.is_empty() && !hosts.contains(&h) {
                hosts.push(h);
            }
        };
        if addr.ip().is_loopback() {
            for local in ["localhost", "127.0.0.1", "::1"] {
                add(local.to_string());
            }
        }
        if !addr.ip().is_unspecified() {
            add(addr.ip().to_string());
        }
        for name in names {
            add(name.clone());
        }
        let port = addr.port();
        let mut origins = Vec::new();
        for host in &hosts {
            let literal = host_literal(host);
            origins.push(format!("http://{literal}:{port}"));
            // Tailscale Serve fronts the listener with HTTPS on the default
            // port (the API) or 8443 (MCP).
            origins.push(format!("https://{literal}"));
            origins.push(format!("https://{literal}:{port}"));
            origins.push(format!("https://{literal}:8443"));
        }
        Self { hosts, origins }
    }

    /// The Host names the listener answers for (lowercase, no port, IPv6 in
    /// brackets), for `ServerConfig::with_allowed_hosts`.
    #[must_use]
    pub fn hosts(&self) -> &[String] {
        &self.hosts
    }

    /// Whether `origin` is one of the daemon's own.
    #[must_use]
    pub fn allows_origin(&self, origin: &str) -> bool {
        let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
        self.origins.contains(&origin)
    }

    /// Check a request against the Origin rule and, for `write` routes, the
    /// JSON rule.
    pub fn admit(&self, req: &Request, write: bool) -> Result<(), Failure> {
        let header = |name: &str| {
            req.headers()
                .get(name)
                .map(|v| String::from_utf8_lossy(v).into_owned())
        };
        if let Some(origin) = header("origin")
            && !self.allows_origin(&origin)
        {
            return Err(Failure::new(
                ErrorCode::UntrustedOrigin,
                format!("requests from the web origin {origin:?} are not accepted"),
            ));
        }
        if write {
            let json = header("content-type").is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|m| m.trim().eq_ignore_ascii_case("application/json"))
            });
            if !json {
                return Err(Failure::new(
                    ErrorCode::UnsupportedMediaType,
                    "control requests must send Content-Type: application/json",
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi::Method;

    fn request(headers: &[(&str, &str)]) -> Request {
        let mut req = Request::new(Method::Post, "/pause");
        for (name, value) in headers {
            req.headers_mut().insert(*name, value.as_bytes().to_vec());
        }
        req
    }

    fn loopback() -> WebPolicy {
        WebPolicy::for_listener(
            "127.0.0.1:8099".parse().unwrap(),
            &["host.tailnet-name.ts.net".into(), "100.70.1.2".into()],
        )
    }

    #[test]
    fn hosts_are_the_listeners_own_names() {
        let web = loopback();
        assert_eq!(
            web.hosts(),
            [
                "localhost",
                "127.0.0.1",
                "[::1]",
                "host.tailnet-name.ts.net",
                "100.70.1.2"
            ]
        );
        let tailnet = WebPolicy::for_listener("100.70.1.2:8099".parse().unwrap(), &[]);
        assert_eq!(tailnet.hosts(), ["100.70.1.2"]);
    }

    /// Every host is a pattern fastapi's Host check can match; an IPv6
    /// listener answers a client's `Host: [addr]:port`.
    #[test]
    fn ipv6_hosts_match_the_host_header_clients_send() {
        let v6 = "fd7a:115c:a1e0::6501:6667";
        let web = WebPolicy::for_listener(
            format!("[{v6}]:8099").parse().unwrap(),
            &["host.tailnet-name.ts.net".into(), v6.into(), "::1".into()],
        );
        assert_eq!(
            web.hosts(),
            [
                format!("[{v6}]").as_str(),
                "host.tailnet-name.ts.net",
                "[::1]"
            ]
        );
        for host in loopback().hosts().iter().chain(web.hosts()) {
            assert!(
                fastapi::RequestAuthority::parse(host).is_some(),
                "{host} is not a Host pattern fastapi parses"
            );
        }
        let sent = fastapi::RequestAuthority::parse(&format!("[{v6}]:8099")).unwrap();
        let allowed = fastapi::RequestAuthority::parse(&web.hosts()[0]).unwrap();
        assert_eq!(sent.host(), allowed.host());
        assert!(web.allows_origin(&format!("http://[{v6}]:8099")));
    }

    #[test]
    fn only_the_daemons_own_origins_pass() {
        let web = loopback();
        for ok in [
            "http://127.0.0.1:8099",
            "http://localhost:8099/",
            "http://[::1]:8099",
            "https://host.tailnet-name.ts.net",
            "https://host.tailnet-name.ts.net:8443",
        ] {
            assert!(web.allows_origin(ok), "{ok}");
        }
        for bad in [
            "https://evil.example",
            "http://127.0.0.1:3000",
            "null",
            "http://host.tailnet-name.ts.net.evil.example",
        ] {
            assert!(!web.allows_origin(bad), "{bad}");
        }
    }

    #[test]
    fn writes_must_be_json_and_foreign_origins_are_refused() {
        let web = loopback();
        assert_eq!(
            web.admit(&request(&[("content-type", "application/json")]), true),
            Ok(())
        );
        assert_eq!(
            web.admit(
                &request(&[("content-type", "application/json; charset=utf-8")]),
                true
            ),
            Ok(())
        );
        let plain = web
            .admit(&request(&[("content-type", "text/plain")]), true)
            .unwrap_err();
        assert_eq!(
            (plain.code, plain.status()),
            (ErrorCode::UnsupportedMediaType, 415)
        );
        assert_eq!(web.admit(&request(&[]), true).unwrap_err().status(), 415);
        // Reads need no content type.
        assert_eq!(web.admit(&request(&[]), false), Ok(()));
        let foreign = web
            .admit(
                &request(&[
                    ("origin", "https://evil.example"),
                    ("content-type", "application/json"),
                ]),
                true,
            )
            .unwrap_err();
        assert_eq!(
            (foreign.code, foreign.status()),
            (ErrorCode::UntrustedOrigin, 403)
        );
        assert_eq!(
            web.admit(&request(&[("origin", "https://evil.example")]), false)
                .unwrap_err()
                .status(),
            403
        );
    }
}
