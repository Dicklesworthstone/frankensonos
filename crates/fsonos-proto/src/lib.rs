//! Sonos interoperability protocol layer.
//!
//! This crate speaks the same local protocols the official apps and the mature
//! open-source controllers (SoCo, node-sonos, Home Assistant) use to talk to
//! Sonos players you own on your own LAN:
//!
//! * [`ssdp`] — SSDP (UDP multicast 239.255.255.250:1900) device discovery.
//! * [`soap`] — UPnP/SOAP control requests to a player's :1400 services
//!   (AVTransport, RenderingControl, GroupRenderingControl,
//!   ZoneGroupTopology, ContentDirectory, MusicServices, ...).
//! * [`gena`] — GENA event subscriptions (SUBSCRIBE + NOTIFY callback sink).
//! * [`didl`] — DIDL-Lite metadata: parsing, and renderer-facing URI construction.
//! * [`topology`] — ZoneGroupTopology: groups, coordinators, stereo pairs.
//! * [`content`] — ContentDirectory browsing (favorites, queue).
//! * [`description`] — UPnP device descriptions (model, room, S1/S2).
//! * [`control`] — transport, queue, grouping, volume and mute actions, and
//!   the reads that report playback state.
//! * [`net`] — the real LAN transport over asupersync.
//!
//! The I/O lives behind the [`Transport`] trait so pure encoding/decoding is
//! testable without a network, and so the franken async stack is wired in one
//! place ([`net`]).

pub mod content;
pub mod control;
pub mod description;
pub mod didl;
pub mod gena;
pub mod mdns;
pub mod net;
pub mod soap;
pub mod ssdp;
pub mod topology;
mod xml;

use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("transport not yet wired (FND-DEPS): {0}")]
    NotWired(&'static str),
    #[error("soap fault {code}: {reason}")]
    SoapFault { code: u16, reason: String },
    #[error("malformed response: {0}")]
    Malformed(String),
    /// The request never got a usable answer: connection refused or timed
    /// out, or an HTTP status other than the 200 / 500-with-fault Sonos sends.
    #[error("{target}: {detail}")]
    Network { target: String, detail: String },
}

/// The largest body [`Transport::http_get_bytes`] returns: album art is tens
/// of kilobytes, and nothing a player serves needs more.
pub const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// A body fetched with [`Transport::http_get_bytes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpBody {
    /// The `Content-Type` header, when the server sent one.
    pub content_type: Option<String>,
    pub body: Vec<u8>,
}

/// What both discovery channels found in one search window (see
/// [`Transport::discover`]).
#[derive(Debug)]
pub struct Discovered {
    pub ssdp: Result<Vec<ssdp::Advert>, ProtoError>,
    pub mdns: Result<Vec<mdns::SonosAdvert>, ProtoError>,
}

/// Minimal network transport the protocol layer needs. [`net::Lan`] is the
/// real implementation; pure in-memory impls back the unit and golden tests,
/// which only need [`Transport::soap_post`].
pub trait Transport {
    /// POST a SOAP envelope to `http://{host}:1400{control_path}` and return the
    /// raw response body. UPnP faults arrive as HTTP 500 with a SOAP Fault
    /// body: return that body too, so [`soap::parse_response`] can surface the
    /// UPnP error code as [`ProtoError::SoapFault`].
    fn soap_post(
        &self,
        host: std::net::IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError>;

    /// GET `url` (a device description) and return the body.
    fn http_get(&self, _url: &str) -> Result<String, ProtoError> {
        Err(ProtoError::NotWired("http_get"))
    }

    /// GET `url` (album art a player serves, say) and return the body as
    /// bytes, with its content type. At most [`MAX_BODY_BYTES`].
    fn http_get_bytes(&self, _url: &str) -> Result<HttpBody, ProtoError> {
        Err(ProtoError::NotWired("http_get_bytes"))
    }

    /// Multicast an SSDP `M-SEARCH` for ZonePlayers and collect the replies
    /// that arrive within `wait`, one per player.
    fn ssdp_search(&self, _mx_secs: u8, _wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        Err(ProtoError::NotWired("ssdp_search"))
    }

    /// Multicast an mDNS query for `_sonos._tcp.local` and collect the Sonos
    /// advertisements that arrive within `wait`: the second discovery
    /// channel next to [`Self::ssdp_search`]. `addr` carries the player's
    /// IPv4; `household` (S2 TXT `hhid=`) refines what SSDP's USN hinted.
    fn mdns_search(&self, _wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        Err(ProtoError::NotWired("mdns_search"))
    }

    /// Both discovery channels, [`Self::ssdp_search`] and
    /// [`Self::mdns_search`], each for `wait`: one after the other, unless
    /// the transport listens on both at once ([`net::Lan`] does, so a
    /// survey waits once). A wrapper that forwards both searches but not
    /// this one still finds on both channels, through its own searches.
    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        Discovered {
            ssdp: self.ssdp_search(mx_secs, wait),
            mdns: self.mdns_search(wait),
        }
    }
}

/// A shared transport is a transport: one [`net::Lan`] can serve the
/// daemon's surfaces and its live model.
impl<T: Transport + ?Sized> Transport for std::sync::Arc<T> {
    fn soap_post(
        &self,
        host: std::net::IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        (**self).soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        (**self).http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        (**self).http_get_bytes(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        (**self).ssdp_search(mx_secs, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
        (**self).mdns_search(wait)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        (**self).discover(mx_secs, wait)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Answers every channel a transport has, each in its own way.
    struct Everything;

    impl Transport for Everything {
        fn soap_post(
            &self,
            _host: std::net::IpAddr,
            _control_path: &str,
            _soap_action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            Ok("soap".into())
        }

        fn http_get(&self, _url: &str) -> Result<String, ProtoError> {
            Ok("description".into())
        }

        fn http_get_bytes(&self, _url: &str) -> Result<HttpBody, ProtoError> {
            Ok(HttpBody {
                content_type: Some("image/png".into()),
                body: vec![1, 2, 3],
            })
        }

        fn ssdp_search(
            &self,
            _mx_secs: u8,
            _wait: Duration,
        ) -> Result<Vec<ssdp::Advert>, ProtoError> {
            Ok(vec![ssdp::Advert {
                location: "http://192.0.2.10:1400/xml/device_description.xml".into(),
                st: "urn:schemas-upnp-org:device:ZonePlayer:1".into(),
                usn: None,
                household: None,
                boot_seq: None,
            }])
        }

        fn mdns_search(&self, _wait: Duration) -> Result<Vec<mdns::SonosAdvert>, ProtoError> {
            Ok(vec![mdns::SonosAdvert {
                instance: "RINCON_000000000000001400@Living Room".into(),
                uuid: None,
                household: Some("Sonos_example".into()),
                boot_seq: None,
                location: None,
                port: Some(1443),
                addr: Some(std::net::Ipv4Addr::new(192, 0, 2, 10)),
            }])
        }
    }

    #[test]
    fn a_shared_transport_is_the_one_it_shares_on_every_channel() {
        let shared: Arc<dyn Transport + Send + Sync> = Arc::new(Everything);
        let host = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 10));
        assert_eq!(shared.soap_post(host, "/", "a", "").unwrap(), "soap");
        assert_eq!(shared.http_get("http://x/").unwrap(), "description");
        assert_eq!(shared.http_get_bytes("http://x/").unwrap().body, [1, 2, 3]);
        assert_eq!(shared.ssdp_search(1, Duration::ZERO).unwrap().len(), 1);
        // The second discovery channel too: a daemon's shared LAN finds by
        // mDNS what SSDP missed.
        let seen = shared.mdns_search(Duration::ZERO).unwrap();
        assert_eq!(seen[0].household.as_deref(), Some("Sonos_example"));
    }

    /// Listens on both channels at once (as `net::Lan` does), and says
    /// so by what it finds.
    struct AtOnce;
    impl Transport for AtOnce {
        fn soap_post(
            &self,
            _host: std::net::IpAddr,
            _control_path: &str,
            _soap_action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            Err(ProtoError::NotWired("soap_post"))
        }

        fn discover(&self, _mx_secs: u8, _wait: Duration) -> Discovered {
            Discovered {
                ssdp: Ok(Vec::new()),
                mdns: Err(ProtoError::NotWired("at once")),
            }
        }
    }

    #[test]
    fn discovery_runs_both_channels_and_a_shared_transport_keeps_its_own() {
        // By default, each channel through the transport's own search.
        let found = Everything.discover(1, Duration::ZERO);
        assert_eq!(found.ssdp.unwrap().len(), 1);
        assert_eq!(found.mdns.unwrap().len(), 1);

        let shared: Arc<dyn Transport + Send + Sync> = Arc::new(AtOnce);
        let found = shared.discover(1, Duration::ZERO);
        assert!(found.ssdp.is_ok_and(|s| s.is_empty()));
        assert!(matches!(found.mdns, Err(ProtoError::NotWired("at once"))));
    }
}
