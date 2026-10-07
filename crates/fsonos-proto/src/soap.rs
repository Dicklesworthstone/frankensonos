//! UPnP/SOAP control request construction for a player's :1400 services.
//!
//! Envelope building and response/fault parsing are pure and unit-testable;
//! dispatch goes through the [`crate::Transport`] trait ([`call`]), implemented
//! over the franken HTTP client in bead FND-DEPS.

use crate::didl::xml_escape;
use crate::{ProtoError, Transport, xml};
use std::fmt::Write as _;
use std::net::IpAddr;

/// A Sonos UPnP service and its control endpoint. Control paths and service
/// types are stable and documented by the community (svrooij sonos-api-docs,
/// SoCo). Values are filled in as each service is implemented.
#[derive(Debug, Clone, Copy)]
pub struct Service {
    pub name: &'static str,
    pub service_type: &'static str,
    pub control_path: &'static str,
    pub event_path: &'static str,
}

/// AVTransport — play/pause/next/seek, set the renderer URI, queue control.
pub const AV_TRANSPORT: Service = Service {
    name: "AVTransport",
    service_type: "urn:schemas-upnp-org:service:AVTransport:1",
    control_path: "/MediaRenderer/AVTransport/Control",
    event_path: "/MediaRenderer/AVTransport/Event",
};

/// RenderingControl — volume, mute, bass/treble, loudness.
pub const RENDERING_CONTROL: Service = Service {
    name: "RenderingControl",
    service_type: "urn:schemas-upnp-org:service:RenderingControl:1",
    control_path: "/MediaRenderer/RenderingControl/Control",
    event_path: "/MediaRenderer/RenderingControl/Event",
};

/// ZoneGroupTopology — the household's group structure and member list.
pub const ZONE_GROUP_TOPOLOGY: Service = Service {
    name: "ZoneGroupTopology",
    service_type: "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
    control_path: "/ZoneGroupTopology/Control",
    event_path: "/ZoneGroupTopology/Event",
};

/// ContentDirectory — browse favorites, the queue, playlists, the library.
pub const CONTENT_DIRECTORY: Service = Service {
    name: "ContentDirectory",
    service_type: "urn:schemas-upnp-org:service:ContentDirectory:1",
    control_path: "/MediaServer/ContentDirectory/Control",
    event_path: "/MediaServer/ContentDirectory/Event",
};

/// Build a SOAP 1.1 envelope for `service`/`action` with the given argument
/// XML fragment (already escaped by the caller).
#[must_use]
pub fn envelope(service: &Service, action: &str, args_xml: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body>\
         <u:{action} xmlns:u=\"{st}\">{args_xml}</u:{action}>\
         </s:Body></s:Envelope>",
        action = action,
        st = service.service_type,
    )
}

/// The `SOAPACTION` header value for `service`/`action`.
#[must_use]
pub fn soap_action_header(service: &Service, action: &str) -> String {
    format!("\"{}#{}\"", service.service_type, action)
}

/// Render action arguments as the escaped `<Name>value</Name>` fragment that
/// [`envelope`] embeds. Argument order matters to some Sonos actions, so it is
/// preserved.
#[must_use]
pub fn args_xml(args: &[(&str, &str)]) -> String {
    args.iter().fold(String::new(), |mut out, (name, value)| {
        let _ = write!(out, "<{name}>{}</{name}>", xml_escape(value));
        out
    })
}

/// The out-arguments of a successful SOAP action, in document order, with
/// their text already entity-decoded (so an embedded document such as
/// `ZoneGroupState` or a DIDL-Lite `Result` comes back as plain XML).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapResponse {
    pub args: Vec<(String, String)>,
}

impl SoapResponse {
    /// The out-argument `name`, if present.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&str> {
        self.args
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// The out-argument `name`, or a [`ProtoError::Malformed`] naming it.
    pub fn require(&self, name: &str) -> Result<&str, ProtoError> {
        self.get(name).ok_or_else(|| {
            ProtoError::Malformed(format!("response is missing out-argument {name}"))
        })
    }

    /// The out-argument `name` parsed as an unsigned integer.
    pub fn require_u32(&self, name: &str) -> Result<u32, ProtoError> {
        let raw = self.require(name)?;
        raw.trim().parse().map_err(|_| {
            ProtoError::Malformed(format!("out-argument {name} is not a u32: {raw:?}"))
        })
    }
}

/// Parse the response body of `action`. A UPnP fault (which Sonos sends with
/// HTTP 500) becomes [`ProtoError::SoapFault`] carrying the UPnP `errorCode`.
pub fn parse_response(body: &str, action: &str) -> Result<SoapResponse, ProtoError> {
    let doc = xml::parse(body)?;
    let envelope = doc.root_element();
    if envelope.tag_name().name() != "Envelope" {
        return Err(ProtoError::Malformed(format!(
            "expected a SOAP Envelope, got <{}>",
            envelope.tag_name().name()
        )));
    }
    let soap_body = xml::child(envelope, "Body")
        .ok_or_else(|| ProtoError::Malformed("SOAP Envelope has no Body".into()))?;
    if let Some(fault) = xml::child(soap_body, "Fault") {
        return Err(fault_error(fault));
    }
    let expected = format!("{action}Response");
    let response = xml::child(soap_body, &expected)
        .ok_or_else(|| ProtoError::Malformed(format!("SOAP Body has no {expected}")))?;
    let args = response
        .children()
        .filter(roxmltree::Node::is_element)
        .map(|arg| {
            (
                arg.tag_name().name().to_string(),
                arg.text().unwrap_or("").to_string(),
            )
        })
        .collect();
    Ok(SoapResponse { args })
}

fn fault_error(fault: roxmltree::Node<'_, '_>) -> ProtoError {
    let upnp = xml::child(fault, "detail").and_then(|d| xml::child(d, "UPnPError"));
    let code = upnp
        .and_then(|e| xml::child_text(e, "errorCode"))
        .and_then(|c| c.trim().parse().ok())
        .unwrap_or(0);
    let reason = upnp
        .and_then(|e| xml::child_text_nonempty(e, "errorDescription"))
        .or_else(|| xml::child_text_nonempty(fault, "faultstring"))
        .unwrap_or("SOAP fault")
        .to_string();
    ProtoError::SoapFault { code, reason }
}

/// Invoke `service`/`action` on the player at `host` and parse the response.
pub fn call<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    service: &Service,
    action: &str,
    args: &str,
) -> Result<SoapResponse, ProtoError> {
    let body = transport.soap_post(
        host,
        service.control_path,
        &soap_action_header(service, action),
        &envelope(service, action, args),
    )?;
    parse_response(&body, action)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_play_envelope() {
        let env = envelope(
            &AV_TRANSPORT,
            "Play",
            "<InstanceID>0</InstanceID><Speed>1</Speed>",
        );
        assert!(env.contains("urn:schemas-upnp-org:service:AVTransport:1"));
        assert!(env.contains("<u:Play"));
    }

    #[test]
    fn soap_action_format() {
        assert_eq!(
            soap_action_header(&AV_TRANSPORT, "Play"),
            "\"urn:schemas-upnp-org:service:AVTransport:1#Play\""
        );
    }

    #[test]
    fn args_are_escaped_and_ordered() {
        assert_eq!(
            args_xml(&[("ObjectID", "FV:2"), ("Filter", "a&b")]),
            "<ObjectID>FV:2</ObjectID><Filter>a&amp;b</Filter>"
        );
    }

    #[test]
    fn parses_out_arguments_with_decoded_text() {
        let body = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                    <u:GetVolumeResponse xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\">\
                    <CurrentVolume>17</CurrentVolume><Doc>&lt;a b=&quot;1&quot;/&gt;</Doc>\
                    </u:GetVolumeResponse></s:Body></s:Envelope>";
        let r = parse_response(body, "GetVolume").unwrap();
        assert_eq!(r.require_u32("CurrentVolume").unwrap(), 17);
        assert_eq!(r.get("Doc"), Some("<a b=\"1\"/>"));
        assert!(matches!(r.require("Nope"), Err(ProtoError::Malformed(_))));
    }

    #[test]
    fn wrong_action_response_is_malformed() {
        let body = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                    <u:PlayResponse xmlns:u=\"urn:x\"/></s:Body></s:Envelope>";
        assert!(matches!(
            parse_response(body, "Pause"),
            Err(ProtoError::Malformed(m)) if m.contains("PauseResponse")
        ));
        assert!(parse_response("not xml", "Play").is_err());
    }

    #[test]
    fn fault_without_upnp_detail_keeps_faultstring() {
        let body = "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><s:Fault>\
                    <faultcode>s:Server</faultcode><faultstring>Internal</faultstring></s:Fault></s:Body></s:Envelope>";
        match parse_response(body, "Play") {
            Err(ProtoError::SoapFault { code, reason }) => {
                assert_eq!(code, 0);
                assert_eq!(reason, "Internal");
            }
            other => panic!("expected a fault, got {other:?}"),
        }
    }
}
