//! UPnP/SOAP control request construction for a player's :1400 services.
//!
//! Envelope building is pure and unit-testable; dispatch goes through the
//! [`crate::Transport`] trait, implemented over the franken HTTP client in
//! bead FND-DEPS.

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
}
