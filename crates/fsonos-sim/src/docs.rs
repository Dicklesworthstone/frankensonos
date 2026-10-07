//! The XML documents the virtual players serve, shaped like the scrubbed
//! real bodies in `fsonos-proto/tests/fixtures/` (same element order, the
//! same escaping layers) but built from synthetic identifiers.

use crate::SimModel;
use crate::model::{Favorite, Player, QueueItem, State};
use fsonos_proto::didl::xml_escape;
use std::fmt::Write as _;

/// One service a virtual player exposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ServiceDef {
    pub name: &'static str,
    pub service_type: &'static str,
    pub control: &'static str,
    pub event: &'static str,
}

const fn service(
    name: &'static str,
    service_type: &'static str,
    control: &'static str,
    event: &'static str,
) -> ServiceDef {
    ServiceDef {
        name,
        service_type,
        control,
        event,
    }
}

pub(crate) const DEVICE_PROPERTIES: ServiceDef = service(
    "DeviceProperties",
    "urn:schemas-upnp-org:service:DeviceProperties:1",
    "/DeviceProperties/Control",
    "/DeviceProperties/Event",
);
pub(crate) const ZONE_GROUP_TOPOLOGY: ServiceDef = service(
    "ZoneGroupTopology",
    "urn:schemas-upnp-org:service:ZoneGroupTopology:1",
    "/ZoneGroupTopology/Control",
    "/ZoneGroupTopology/Event",
);
pub(crate) const CONTENT_DIRECTORY: ServiceDef = service(
    "ContentDirectory",
    "urn:schemas-upnp-org:service:ContentDirectory:1",
    "/MediaServer/ContentDirectory/Control",
    "/MediaServer/ContentDirectory/Event",
);
pub(crate) const RENDERING_CONTROL: ServiceDef = service(
    "RenderingControl",
    "urn:schemas-upnp-org:service:RenderingControl:1",
    "/MediaRenderer/RenderingControl/Control",
    "/MediaRenderer/RenderingControl/Event",
);
pub(crate) const AV_TRANSPORT: ServiceDef = service(
    "AVTransport",
    "urn:schemas-upnp-org:service:AVTransport:1",
    "/MediaRenderer/AVTransport/Control",
    "/MediaRenderer/AVTransport/Event",
);
pub(crate) const GROUP_RENDERING_CONTROL: ServiceDef = service(
    "GroupRenderingControl",
    "urn:schemas-upnp-org:service:GroupRenderingControl:1",
    "/MediaRenderer/GroupRenderingControl/Control",
    "/MediaRenderer/GroupRenderingControl/Event",
);

/// Top-level services every player (bridges included) exposes.
const TOP_LEVEL: [ServiceDef; 2] = [DEVICE_PROPERTIES, ZONE_GROUP_TOPOLOGY];
/// MediaServer services (renderers only).
const MEDIA_SERVER: [ServiceDef; 1] = [CONTENT_DIRECTORY];
/// MediaRenderer services (renderers only).
const MEDIA_RENDERER: [ServiceDef; 3] = [RENDERING_CONTROL, AV_TRANSPORT, GROUP_RENDERING_CONTROL];

/// The services `model` exposes, per the live service matrix
/// (docs/PROTOCOL.md §2): a Bridge has no MediaServer / MediaRenderer.
pub(crate) fn services(model: SimModel) -> Vec<ServiceDef> {
    let mut all = TOP_LEVEL.to_vec();
    if model.is_renderer() {
        all.extend(MEDIA_SERVER);
        all.extend(MEDIA_RENDERER);
    }
    all
}

fn service_list(out: &mut String, defs: &[ServiceDef]) {
    out.push_str("<serviceList>");
    for d in defs {
        let id = d.service_type.split(':').nth(3).unwrap_or(d.name);
        let _ = write!(
            out,
            "<service><serviceType>{}</serviceType><serviceId>urn:upnp-org:serviceId:{id}</serviceId>\
             <controlURL>{}</controlURL><eventSubURL>{}</eventSubURL><SCPDURL>/xml/{id}1.xml</SCPDURL></service>",
            d.service_type, d.control, d.event
        );
    }
    out.push_str("</serviceList>");
}

/// `/xml/device_description.xml` for `p`.
pub(crate) fn device_description(p: &Player, sw_gen: u8) -> String {
    let m = p.model;
    let mac = p.mac();
    let serial = mac.replace(':', "-");
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"utf-8\" ?>\
         <root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion>\
         <device><deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType>",
    );
    let _ = write!(
        out,
        "<friendlyName>{ip} - {name}</friendlyName><manufacturer>Sonos, Inc.</manufacturer>\
         <manufacturerURL>http://www.sonos.com</manufacturerURL><modelNumber>{number}</modelNumber>\
         <modelDescription>{name}</modelDescription><modelName>{name}</modelName>\
         <modelURL>http://www.sonos.com/products/zoneplayers/{number}</modelURL>\
         <softwareVersion>{version}</softwareVersion><swGen>{sw_gen}</swGen>\
         <hardwareVersion>1.0.0.0-1.0</hardwareVersion><serialNum>{serial}:1</serialNum>\
         <MACAddress>{mac}</MACAddress><UDN>uuid:{uuid}</UDN><displayVersion>{display}</displayVersion>\
         <roomName>{room}</roomName><displayName>{short}</displayName>",
        ip = p.ip,
        name = m.model_name(),
        number = m.model_number(),
        version = software_version(sw_gen),
        display = display_version(sw_gen),
        uuid = p.uuid,
        room = xml_escape(&p.room),
        short = m.display_name(),
    );
    service_list(&mut out, &TOP_LEVEL);
    if m.is_renderer() {
        out.push_str("<deviceList>");
        for (kind, defs) in [
            ("MediaServer", &MEDIA_SERVER[..]),
            ("MediaRenderer", &MEDIA_RENDERER[..]),
        ] {
            let _ = write!(
                out,
                "<device><deviceType>urn:schemas-upnp-org:device:{kind}:1</deviceType>\
                 <friendlyName>{room} - {name} {kind}</friendlyName><UDN>uuid:{uuid}_{tag}</UDN>",
                room = xml_escape(&p.room),
                name = m.model_name(),
                uuid = p.uuid,
                tag = if kind == "MediaServer" { "MS" } else { "MR" },
            );
            service_list(&mut out, defs);
            out.push_str("</device>");
        }
        out.push_str("</deviceList>");
    }
    out.push_str("</device></root>");
    out
}

pub(crate) fn software_version(sw_gen: u8) -> &'static str {
    if sw_gen == 1 {
        "57.23-74170"
    } else {
        "86.10-80260"
    }
}

fn display_version(sw_gen: u8) -> &'static str {
    if sw_gen == 1 { "11.16.1" } else { "17.2.7" }
}

/// The decoded `ZoneGroupState` document for household `h`.
pub(crate) fn zone_group_state(state: &State, h: usize) -> String {
    let mut out = String::from("<ZoneGroupState><ZoneGroups>");
    let sw_gen = state.households[h].sw_gen;
    for coord in state.coordinators(h) {
        let c = &state.players[coord];
        if c.offline {
            continue;
        }
        let _ = write!(
            out,
            "<ZoneGroup Coordinator=\"{}\" ID=\"{}\">",
            c.uuid, c.group_id
        );
        for &m in &state.members(coord) {
            let p = &state.players[m];
            if p.offline {
                continue;
            }
            // A stereo pair: both halves carry the channel map; the RF half
            // is hidden (the S1 fixture's shape).
            let pair = p
                .pair_primary
                .map(|primary| (primary, m))
                .or_else(|| state.secondary_of(m).map(|secondary| (m, secondary)));
            let mut extra = String::new();
            if !p.model.is_renderer() {
                extra.push_str(" Invisible=\"1\" IsZoneBridge=\"1\"");
            } else if p.pair_primary.is_some() {
                extra.push_str(" Invisible=\"1\"");
            }
            if let Some((lf, rf)) = pair {
                let _ = write!(
                    extra,
                    " ChannelMapSet=\"{}:LF,LF;{}:RF,RF\"",
                    state.players[lf].uuid, state.players[rf].uuid
                );
            }
            let _ = write!(
                out,
                "<ZoneGroupMember UUID=\"{uuid}\" Location=\"{location}\" ZoneName=\"{room}\" \
                 Icon=\"\" Configuration=\"1\"{extra} SoftwareVersion=\"{version}\" SWGen=\"{sw_gen}\" \
                 BootSeq=\"{boot}\" IdleState=\"1\" MoreInfo=\"\"/>",
                uuid = p.uuid,
                location = p.location(),
                room = xml_escape(&p.room),
                version = software_version(sw_gen),
                boot = p.boot_seq,
            );
        }
        out.push_str("</ZoneGroup>");
    }
    out.push_str("</ZoneGroups><VanishedDevices>");
    // Powered-off players, in the live fixture's shape.
    for p in state
        .players
        .iter()
        .filter(|p| p.household == h && p.offline)
    {
        let _ = write!(
            out,
            "<Device UUID=\"{uuid}\" ZoneName=\"{room}\" Reason=\"powered off\" ModelInfo=\"{model}\" \
             Mac=\"{mac}\" LastKnownIP=\"{ip}\" MoreInfo=\"\" SWGen=\"{sw_gen}\"/>",
            uuid = p.uuid,
            room = xml_escape(&p.room),
            model = p.model.model_number(),
            mac = p.mac(),
            ip = p.ip,
        );
    }
    out.push_str("</VanishedDevices></ZoneGroupState>");
    out
}

const DIDL_OPEN: &str = "<DIDL-Lite xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
     xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
     xmlns:r=\"urn:schemas-rinconnetworks-com:metadata-1-0/\" \
     xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\">";

/// Wrap DIDL items in a `DIDL-Lite` root.
pub(crate) fn didl(items: &str) -> String {
    format!("{DIDL_OPEN}{items}</DIDL-Lite>")
}

/// The single-item DIDL a favorite (or a queued track) is replayed with.
pub(crate) fn item_metadata(id: &str, title: &str, class: &str, desc: &str) -> String {
    didl(&format!(
        "<item id=\"{id}\" parentID=\"-1\" restricted=\"true\"><dc:title>{}</dc:title>\
         <upnp:class>{class}</upnp:class>\
         <desc id=\"cdudn\" nameSpace=\"urn:schemas-rinconnetworks-com:metadata-1-0/\">{}</desc></item>",
        xml_escape(title),
        xml_escape(desc)
    ))
}

/// The favorites (`FV:2`) DIDL items, `ordinal` numbered from `start`.
pub(crate) fn favorite_items(favorites: &[Favorite], start: usize) -> String {
    let mut out = String::new();
    for (i, f) in favorites.iter().enumerate() {
        let n = start + i;
        let _ = write!(
            out,
            "<item id=\"FV:2/{n}\" parentID=\"FV:2\" restricted=\"false\"><dc:title>{title}</dc:title>\
             <upnp:class>object.itemobject.item.sonos-favorite</upnp:class><r:ordinal>{n}</r:ordinal>\
             <res protocolInfo=\"{pi}\">{uri}</res><r:type>{kind}</r:type>\
             <r:description>{desc}</r:description><r:resMD>{md}</r:resMD></item>",
            title = xml_escape(&f.title),
            pi = f.protocol_info,
            uri = xml_escape(&f.uri),
            kind = f.kind,
            desc = xml_escape(&f.description),
            md = xml_escape(&f.metadata),
        );
    }
    out
}

/// The queue (`Q:0`) DIDL items, numbered from `start` (1-based).
pub(crate) fn queue_items(queue: &[QueueItem], start: usize) -> String {
    let mut out = String::new();
    for (i, q) in queue.iter().enumerate() {
        let _ = write!(
            out,
            "<item id=\"Q:0/{n}\" parentID=\"Q:0\" restricted=\"true\">\
             <res protocolInfo=\"{pi}\" duration=\"{dur}\">{uri}</res><dc:title>{title}</dc:title>\
             <upnp:class>object.item.audioItem.musicTrack</upnp:class>",
            n = start + i,
            pi = protocol_info(&q.uri),
            dur = hms(q.duration_ms),
            uri = xml_escape(&q.uri),
            title = xml_escape(&q.title),
        );
        if let Some(c) = &q.creator {
            let _ = write!(out, "<dc:creator>{}</dc:creator>", xml_escape(c));
        }
        if let Some(a) = &q.album {
            let _ = write!(out, "<upnp:album>{}</upnp:album>", xml_escape(a));
        }
        out.push_str("</item>");
    }
    out
}

/// The `protocolInfo` Sonos reports for a URI's scheme.
pub(crate) fn protocol_info(uri: &str) -> &'static str {
    if uri.starts_with("x-sonos-spotify:") || uri.starts_with("spotify") {
        "sonos.com-spotify:*:audio/x-spotify:*"
    } else if uri.starts_with("x-rincon-mp3radio:") {
        "x-rincon-mp3radio:*:audio/x-rincon-mp3radio:*"
    } else if uri.starts_with("x-rincon-cpcontainer:") {
        "x-rincon-cpcontainer:*:*:*"
    } else {
        "http-get:*:audio/mpeg:*"
    }
}

/// `H:MM:SS`, as Sonos formats durations and positions.
pub(crate) fn hms(ms: u64) -> String {
    let s = ms / 1000;
    format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A successful SOAP response for `service`/`action`.
pub(crate) fn soap_response(service: &ServiceDef, action: &str, out: &[(&str, String)]) -> String {
    let mut args = String::new();
    for (name, value) in out {
        let _ = write!(args, "<{name}>{}</{name}>", xml_escape(value));
    }
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body>\
         <u:{action}Response xmlns:u=\"{}\">{args}</u:{action}Response></s:Body></s:Envelope>",
        service.service_type
    )
}

/// A UPnP fault body, exactly as real players send it (HTTP 500).
pub(crate) fn soap_fault(code: u16) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault>\
         <faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail>\
         <UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode>\
         </UPnPError></detail></s:Fault></s:Body></s:Envelope>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_formats_like_sonos() {
        assert_eq!(hms(0), "0:00:00");
        assert_eq!(hms(239_999), "0:03:59");
        assert_eq!(hms(3_725_000), "1:02:05");
    }

    #[test]
    fn bridges_expose_no_media_services() {
        let names = |m| services(m).iter().map(|s| s.name).collect::<Vec<_>>();
        assert_eq!(
            names(SimModel::Bridge),
            ["DeviceProperties", "ZoneGroupTopology"]
        );
        assert!(names(SimModel::One).contains(&"AVTransport"));
    }
}
