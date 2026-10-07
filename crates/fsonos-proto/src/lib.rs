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
//!
//! The I/O lives behind the [`Transport`] trait so pure encoding/decoding is
//! testable without a network, and so the franken async stack is wired in one
//! place (bead FND-DEPS).

pub mod content;
pub mod description;
pub mod didl;
pub mod gena;
pub mod soap;
pub mod ssdp;
pub mod topology;
mod xml;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProtoError {
    #[error("transport not yet wired (FND-DEPS): {0}")]
    NotWired(&'static str),
    #[error("soap fault {code}: {reason}")]
    SoapFault { code: u16, reason: String },
    #[error("malformed response: {0}")]
    Malformed(String),
}

/// Minimal network transport the protocol layer needs. Implemented over the
/// franken async stack in [`crate::soap`]/[`crate::ssdp`] once FND-DEPS lands;
/// a pure in-memory impl backs the encoding/decoding unit tests.
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
}
