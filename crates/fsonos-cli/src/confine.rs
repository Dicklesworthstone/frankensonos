//! A routes file confines `fsonos` to what it names.
//!
//! `--routes` (`FSONOS_ROUTES`) exists for the simulator: its players listen
//! on loopback ports behind documentation addresses (`192.0.2.x`). While a
//! routes file is in effect nothing may reach the real LAN, so [`Confined`]
//! refuses, without sending anything:
//!
//! * a request for a player the file does not route (a non-loopback address);
//! * a URL on any other host, or on a routed player's address but another
//!   port than 1400 (which the routes would not redirect);
//! * a multicast SSDP search when the file names no `ssdp` responder.
//!
//! A refusal is a [`ProtoError::Network`] whose detail contains [`REFUSED`],
//! and is printed on stderr as well, so the e2e harness can prove a run
//! touched only the simulator.

use fsonos_proto::net::PLAYER_PORT;
use fsonos_proto::ssdp::Advert;
use fsonos_proto::{ProtoError, Transport};
use std::net::{IpAddr, SocketAddr};
use std::time::Duration;

use crate::config::Routes;

/// The marker in every refusal's detail.
pub const REFUSED: &str = "refused: outside the routes file";

/// `inner`, allowed to reach only what a routes file names. See the module
/// docs.
pub struct Confined<T> {
    inner: T,
    routed: Vec<IpAddr>,
    ssdp: bool,
}

impl<T: Transport> Confined<T> {
    /// `inner` (already carrying `routes`), confined to them.
    pub fn new(inner: T, routes: &Routes) -> Self {
        Self {
            inner,
            routed: routes.players.iter().map(|(ip, _)| *ip).collect(),
            ssdp: routes.ssdp.is_some(),
        }
    }

    fn player(&self, host: IpAddr) -> bool {
        host.is_loopback() || self.routed.contains(&host)
    }

    /// Whether `url` stays on loopback or on a routed player's port 1400.
    fn url(&self, url: &str) -> bool {
        let Some(rest) = url.strip_prefix("http://") else {
            return false;
        };
        let authority = &rest[..rest.find('/').unwrap_or(rest.len())];
        authority.parse::<SocketAddr>().is_ok_and(|addr| {
            addr.ip().is_loopback()
                || (addr.port() == PLAYER_PORT && self.routed.contains(&addr.ip()))
        })
    }
}

/// `Ok` when `ok`; else report and refuse the request for `target`.
fn allow(target: &str, ok: bool) -> Result<(), ProtoError> {
    if ok {
        return Ok(());
    }
    eprintln!("fsonos: {target}: {REFUSED}");
    Err(ProtoError::Network {
        target: target.to_string(),
        detail: format!("{REFUSED} (FSONOS_ROUTES names only the simulator's players)"),
    })
}

impl<T: Transport> Transport for Confined<T> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        allow(&host.to_string(), self.player(host))?;
        self.inner.soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        allow(url, self.url(url))?;
        self.inner.http_get(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        allow("SSDP multicast", self.ssdp)?;
        self.inner.ssdp_search(mx_secs, wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Records what reaches it.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    impl Transport for Recorder {
        fn soap_post(&self, host: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            self.0.lock().unwrap().push(format!("soap {host}"));
            Ok(String::new())
        }

        fn http_get(&self, url: &str) -> Result<String, ProtoError> {
            self.0.lock().unwrap().push(format!("get {url}"));
            Ok(String::new())
        }

        fn ssdp_search(&self, _: u8, _: Duration) -> Result<Vec<Advert>, ProtoError> {
            self.0.lock().unwrap().push("ssdp".into());
            Ok(Vec::new())
        }
    }

    fn confined(ssdp: bool) -> Confined<Recorder> {
        let routes = Routes {
            players: vec![(
                "192.0.2.10".parse().unwrap(),
                "127.0.0.1:53211".parse().unwrap(),
            )],
            ssdp: ssdp.then(|| "127.0.0.1:53000".parse().unwrap()),
        };
        Confined::new(Recorder::default(), &routes)
    }

    fn refused(r: Result<impl std::fmt::Debug, ProtoError>) -> bool {
        matches!(r, Err(ProtoError::Network { detail, .. }) if detail.contains(REFUSED))
    }

    #[test]
    fn only_routed_players_and_loopback_are_reached() {
        let t = confined(true);
        let routed: IpAddr = "192.0.2.10".parse().unwrap();
        assert!(t.soap_post(routed, "/x", "a", "").is_ok());
        assert!(
            t.soap_post("127.0.0.1".parse().unwrap(), "/x", "a", "")
                .is_ok()
        );
        assert!(refused(t.soap_post(
            "192.0.2.11".parse().unwrap(),
            "/x",
            "a",
            ""
        )));
        assert!(refused(t.soap_post(
            "10.0.0.5".parse().unwrap(),
            "/x",
            "a",
            ""
        )));

        assert!(
            t.http_get("http://192.0.2.10:1400/xml/device_description.xml")
                .is_ok()
        );
        assert!(
            t.http_get("http://127.0.0.1:53211/xml/device_description.xml")
                .is_ok()
        );
        for outside in [
            "http://192.0.2.10:8080/other",
            "http://192.0.2.99:1400/xml/device_description.xml",
            "http://players.example:1400/",
            "https://192.0.2.10:1400/",
        ] {
            assert!(refused(t.http_get(outside)), "{outside}");
        }

        assert!(t.ssdp_search(1, Duration::ZERO).is_ok());
        assert!(refused(confined(false).ssdp_search(1, Duration::ZERO)));

        // Nothing refused was sent.
        let sent = t.inner.0.lock().unwrap().clone();
        assert_eq!(
            sent,
            [
                "soap 192.0.2.10",
                "soap 127.0.0.1",
                "get http://192.0.2.10:1400/xml/device_description.xml",
                "get http://127.0.0.1:53211/xml/device_description.xml",
                "ssdp",
            ]
        );
    }
}
