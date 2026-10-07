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

    /// Multicast an SSDP `M-SEARCH` for ZonePlayers and collect the replies
    /// that arrive within `wait`, one per player.
    fn ssdp_search(&self, _mx_secs: u8, _wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        Err(ProtoError::NotWired("ssdp_search"))
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

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<ssdp::Advert>, ProtoError> {
        (**self).ssdp_search(mx_secs, wait)
    }
}
