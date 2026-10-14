//! UPnP device descriptions (`http://<ip>:1400/xml/device_description.xml`).
//!
//! The description identifies a player (UDN), its room, its hardware model,
//! its software line (`swGen`: 1 = S1, 2 = S2), and the services it exposes.
//! Zone bridges expose no MediaRenderer services and cannot play audio.

use crate::{ProtoError, soap, xml};
use fsonos_types::PlayerId;

/// Path of the description document on every player.
pub const DESCRIPTION_PATH: &str = "/xml/device_description.xml";

/// The fields of a device description FrankenSonos uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceDescription {
    /// The player id (`RINCON_…`), with the UDN's `uuid:` prefix removed.
    pub udn: PlayerId,
    pub room_name: String,
    /// Short product name, e.g. `Play:5` or `One`.
    pub display_name: Option<String>,
    /// Marketing model name, e.g. `Sonos Play:5`.
    pub model_name: String,
    /// Hardware model number, e.g. `S5` (Play:5 Gen 1) or `ZB100` (Bridge).
    pub model_number: String,
    pub software_version: Option<String>,
    /// User-facing release, e.g. `11.16.1` (S1) or `17.2.7` (S2).
    pub display_version: Option<String>,
    /// `swGen`: 1 = S1, 2 = S2.
    pub sw_gen: Option<u8>,
    /// Every service of the device and its embedded devices.
    pub services: Vec<ServiceInfo>,
}

/// One `<service>` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceInfo {
    pub service_type: String,
    pub control_url: String,
    pub event_sub_url: String,
}

impl DeviceDescription {
    /// Whether the device exposes `service_type`.
    #[must_use]
    pub fn has_service(&self, service_type: &str) -> bool {
        self.services.iter().any(|s| s.service_type == service_type)
    }

    /// Whether the device can render audio (has AVTransport). False for zone
    /// bridges.
    #[must_use]
    pub fn is_renderer(&self) -> bool {
        self.has_service(soap::AV_TRANSPORT.service_type)
    }
}

/// Parse a device description document.
pub fn parse_device_description(doc_text: &str) -> Result<DeviceDescription, ProtoError> {
    let doc = xml::parse(doc_text)?;
    let device = xml::child(doc.root_element(), "device")
        .ok_or_else(|| ProtoError::Malformed("device description has no <device>".into()))?;
    let required = |name: &str| {
        xml::child_text_nonempty(device, name)
            .map(|t| t.trim().to_string())
            .ok_or_else(|| ProtoError::Malformed(format!("device description has no <{name}>")))
    };
    let optional =
        |name: &str| xml::child_text_nonempty(device, name).map(|t| t.trim().to_string());
    let udn = required("UDN")?;
    let mut services = Vec::new();
    collect_services(device, &mut services);
    Ok(DeviceDescription {
        udn: PlayerId(udn.strip_prefix("uuid:").unwrap_or(&udn).to_string()),
        room_name: optional("roomName").unwrap_or_default(),
        display_name: optional("displayName"),
        model_name: required("modelName")?,
        model_number: optional("modelNumber").unwrap_or_default(),
        software_version: optional("softwareVersion"),
        display_version: optional("displayVersion"),
        sw_gen: optional("swGen").and_then(|g| g.parse().ok()),
        services,
    })
}

fn collect_services(device: roxmltree::Node<'_, '_>, out: &mut Vec<ServiceInfo>) {
    if let Some(list) = xml::child(device, "serviceList") {
        out.extend(xml::children(list, "service").map(|s| {
            ServiceInfo {
                service_type: xml::child_text(s, "serviceType")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                control_url: xml::child_text(s, "controlURL")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                event_sub_url: xml::child_text(s, "eventSubURL")
                    .unwrap_or("")
                    .trim()
                    .to_string(),
            }
        }));
    }
    if let Some(list) = xml::child(device, "deviceList") {
        for embedded in xml::children(list, "device") {
            collect_services(embedded, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_description_without_optional_fields() {
        let doc = r#"<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
            <UDN>uuid:RINCON_000E58A0000101400</UDN><modelName>Sonos Thing</modelName>
            </device></root>"#;
        let d = parse_device_description(doc).unwrap();
        assert_eq!(d.udn, PlayerId("RINCON_000E58A0000101400".into()));
        assert_eq!(d.sw_gen, None);
        assert_eq!(d.services.len(), 0);
        assert!(!d.is_renderer());
    }

    #[test]
    fn missing_udn_is_malformed() {
        let doc = "<root><device><modelName>x</modelName></device></root>";
        assert!(matches!(
            parse_device_description(doc),
            Err(ProtoError::Malformed(m)) if m.contains("UDN")
        ));
    }
}
