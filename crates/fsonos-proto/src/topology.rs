//! ZoneGroupTopology: the household's group structure.
//!
//! `GetZoneGroupState` (and the `ZoneGroupState` GENA event variable) carries
//! an XML document listing every zone group, its coordinator, and each member
//! player with its room name, location, visibility, and stereo-pair /
//! home-theater channel maps. Current S1 (11.x) and S2 firmware wrap the
//! groups as `<ZoneGroupState><ZoneGroups>…</ZoneGroups><VanishedDevices/>`;
//! older firmware returns a bare `<ZoneGroups>` root. Both are accepted.
//!
//! Observed on real households (see `tests/fixtures/zgs_*.xml`):
//! * a group's `ID` prefix need not be its coordinator's UUID;
//! * the secondary of a stereo pair is `Invisible="1"`, and when the visible
//!   primary is offline the invisible secondary can be the only member left —
//!   and the coordinator;
//! * zone bridges are members of their own single-member groups
//!   (`IsZoneBridge="1"`) and cannot render audio.

use crate::{ProtoError, Transport, soap, xml};
use fsonos_types::PlayerId;
use std::net::IpAddr;

/// The parsed `ZoneGroupState` document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneGroupState {
    pub groups: Vec<ZoneGroupInfo>,
    /// Bonded or known players the household currently cannot see.
    pub vanished: Vec<VanishedDevice>,
}

impl ZoneGroupState {
    /// Each visible member's and satellite's `BootSeq`, where reported.
    pub fn boot_seqs(&self) -> impl Iterator<Item = (&PlayerId, u32)> {
        self.groups
            .iter()
            .flat_map(|g| &g.members)
            .flat_map(|m| std::iter::once(m).chain(&m.satellites))
            .filter_map(|m| Some((&m.uuid, m.boot_seq?)))
    }
}

/// One `<ZoneGroup>`: a coordinator plus the players rendering with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneGroupInfo {
    /// Opaque group id (`RINCON_…:<n>`). Do not derive the coordinator from it.
    pub id: String,
    pub coordinator: PlayerId,
    pub members: Vec<ZoneMember>,
}

/// One `<ZoneGroupMember>` (or a nested home-theater `<Satellite>`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMember {
    pub uuid: PlayerId,
    /// Device-description URL, e.g. `http://<ip>:1400/xml/device_description.xml`.
    pub location: String,
    pub zone_name: String,
    /// Hidden from room lists: the secondary of a stereo pair, a satellite, a bridge.
    pub invisible: bool,
    pub is_zone_bridge: bool,
    /// Software generation the player reports (`SWGen`): 1 = S1, 2 = S2.
    pub sw_gen: Option<u8>,
    pub software_version: Option<String>,
    /// `BootSeq`: increments every time the player boots.
    pub boot_seq: Option<u32>,
    /// `ChannelMapSet`: stereo pair (and attached sub) bonding.
    pub channel_map: Vec<ChannelAssignment>,
    /// `HTSatChanMapSet`: home-theater bonding (soundbar, surrounds, sub).
    pub ht_sat_chan_map: Vec<ChannelAssignment>,
    /// Home-theater satellites nested under this member.
    pub satellites: Vec<ZoneMember>,
}

/// One `RINCON_…:LF,LF` entry of a channel map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChannelAssignment {
    pub player: PlayerId,
    /// The channel spec, e.g. `LF,LF`, `RF,RF`, `SW,SW` or `LF,RF`.
    pub channels: String,
}

/// One `<VanishedDevices><Device>` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VanishedDevice {
    pub uuid: PlayerId,
    pub zone_name: String,
    pub reason: Option<String>,
    /// `ModelInfo`, e.g. `S12` or `Sub`.
    pub model: Option<String>,
    pub last_known_ip: Option<IpAddr>,
}

impl ZoneMember {
    /// The player's IP address, taken from its `Location` URL.
    #[must_use]
    pub fn ip(&self) -> Option<IpAddr> {
        host_of_location(&self.location)
    }

    /// Every player this member's channel maps bond it to, itself included.
    pub fn bonded_players(&self) -> impl Iterator<Item = &PlayerId> {
        std::iter::once(&self.uuid)
            .chain(self.channel_map.iter().map(|c| &c.player))
            .chain(self.ht_sat_chan_map.iter().map(|c| &c.player))
            .chain(self.satellites.iter().map(|s| &s.uuid))
    }
}

/// Extract the host of an `http://host:port/path` URL as an IP address.
#[must_use]
pub fn host_of_location(location: &str) -> Option<IpAddr> {
    let rest = location
        .strip_prefix("http://")
        .or_else(|| location.strip_prefix("https://"))?;
    let authority = rest.split('/').next()?;
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split(']').next()?.parse().ok();
    }
    authority.split(':').next()?.parse().ok()
}

/// Parse a `ChannelMapSet` / `HTSatChanMapSet` value
/// (`RINCON_A01400:LF,LF;RINCON_B01400:RF,RF`).
#[must_use]
pub fn parse_channel_map(value: &str) -> Vec<ChannelAssignment> {
    value
        .split(';')
        .filter_map(|entry| entry.split_once(':'))
        .filter(|(uuid, _)| !uuid.is_empty())
        .map(|(uuid, channels)| ChannelAssignment {
            player: PlayerId(uuid.to_string()),
            channels: channels.to_string(),
        })
        .collect()
}

/// Parse a decoded `ZoneGroupState` document (the SOAP out-argument or the
/// GENA event variable of the same name).
pub fn parse_zone_group_state(doc_text: &str) -> Result<ZoneGroupState, ProtoError> {
    let doc = xml::parse(doc_text)?;
    let root = doc.root_element();
    let (groups_node, vanished_node) = match root.tag_name().name() {
        "ZoneGroupState" => (
            xml::child(root, "ZoneGroups")
                .ok_or_else(|| ProtoError::Malformed("ZoneGroupState has no ZoneGroups".into()))?,
            xml::child(root, "VanishedDevices"),
        ),
        "ZoneGroups" => (root, None),
        other => {
            return Err(ProtoError::Malformed(format!(
                "expected ZoneGroupState or ZoneGroups, got <{other}>"
            )));
        }
    };

    let groups = xml::children(groups_node, "ZoneGroup")
        .map(|g| {
            Ok(ZoneGroupInfo {
                id: g.attribute("ID").unwrap_or_default().to_string(),
                coordinator: PlayerId(xml::require_attr(g, "Coordinator")?.to_string()),
                members: xml::children(g, "ZoneGroupMember")
                    .map(parse_member)
                    .collect::<Result<_, _>>()?,
            })
        })
        .collect::<Result<_, ProtoError>>()?;

    let vanished = vanished_node
        .map(|v| {
            xml::children(v, "Device")
                .map(|d| {
                    Ok(VanishedDevice {
                        uuid: PlayerId(xml::require_attr(d, "UUID")?.to_string()),
                        zone_name: d.attribute("ZoneName").unwrap_or_default().to_string(),
                        reason: d.attribute("Reason").map(str::to_string),
                        model: d.attribute("ModelInfo").map(str::to_string),
                        last_known_ip: d.attribute("LastKnownIP").and_then(|ip| ip.parse().ok()),
                    })
                })
                .collect::<Result<_, ProtoError>>()
        })
        .transpose()?
        .unwrap_or_default();

    Ok(ZoneGroupState { groups, vanished })
}

fn parse_member(node: roxmltree::Node<'_, '_>) -> Result<ZoneMember, ProtoError> {
    let flag = |name: &str| node.attribute(name) == Some("1");
    Ok(ZoneMember {
        uuid: PlayerId(xml::require_attr(node, "UUID")?.to_string()),
        location: xml::require_attr(node, "Location")?.to_string(),
        zone_name: node.attribute("ZoneName").unwrap_or_default().to_string(),
        invisible: flag("Invisible"),
        is_zone_bridge: flag("IsZoneBridge"),
        sw_gen: node.attribute("SWGen").and_then(|g| g.parse().ok()),
        software_version: node.attribute("SoftwareVersion").map(str::to_string),
        boot_seq: node
            .attribute("BootSeq")
            .and_then(|b| b.trim().parse().ok()),
        channel_map: node
            .attribute("ChannelMapSet")
            .map(parse_channel_map)
            .unwrap_or_default(),
        ht_sat_chan_map: node
            .attribute("HTSatChanMapSet")
            .map(parse_channel_map)
            .unwrap_or_default(),
        satellites: xml::children(node, "Satellite")
            .map(parse_member)
            .collect::<Result<_, _>>()?,
    })
}

/// Fetch and parse the household topology from the player at `host`. Any
/// player in the household can answer for all of it.
pub fn get_zone_group_state<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
) -> Result<ZoneGroupState, ProtoError> {
    let response = soap::call(
        transport,
        host,
        &soap::ZONE_GROUP_TOPOLOGY,
        "GetZoneGroupState",
        "",
    )?;
    parse_zone_group_state(response.require("ZoneGroupState")?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn location_host_is_extracted() {
        assert_eq!(
            host_of_location("http://192.0.2.10:1400/xml/device_description.xml"),
            Some("192.0.2.10".parse().unwrap())
        );
        assert_eq!(
            host_of_location("http://[2001:db8::1]:1400/x"),
            Some("2001:db8::1".parse().unwrap())
        );
        assert_eq!(host_of_location("ftp://192.0.2.10/"), None);
        assert_eq!(host_of_location("http://not-an-ip:1400/"), None);
    }

    #[test]
    fn channel_map_entries_split() {
        let m = parse_channel_map("RINCON_A01400:LF,LF;RINCON_B01400:RF,RF;");
        assert_eq!(m.len(), 2);
        assert_eq!(m[1].player, PlayerId("RINCON_B01400".into()));
        assert_eq!(m[1].channels, "RF,RF");
        assert_eq!(parse_channel_map("").len(), 0);
    }

    #[test]
    fn boot_seq_is_read_from_each_member() {
        let doc = r#"<ZoneGroupState><ZoneGroups><ZoneGroup Coordinator="RINCON_A01400" ID="RINCON_A01400:1">
            <ZoneGroupMember UUID="RINCON_A01400" Location="http://192.0.2.5:1400/xml/device_description.xml"
              ZoneName="Den" BootSeq="17"/></ZoneGroup></ZoneGroups></ZoneGroupState>"#;
        let zgs = parse_zone_group_state(doc).unwrap();
        assert_eq!(zgs.groups[0].members[0].boot_seq, Some(17));
        let seqs: Vec<_> = zgs.boot_seqs().map(|(p, s)| (p.0.as_str(), s)).collect();
        assert_eq!(seqs, [("RINCON_A01400", 17)]);
    }

    #[test]
    fn legacy_bare_zone_groups_root_with_satellites() {
        let doc = r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_SB01400" ID="RINCON_SB01400:7">
            <ZoneGroupMember UUID="RINCON_SB01400" Location="http://192.0.2.5:1400/xml/device_description.xml"
              ZoneName="TV Room" HTSatChanMapSet="RINCON_SB01400:LF,RF;RINCON_SUB01400:SW">
              <Satellite UUID="RINCON_SUB01400" Location="http://192.0.2.6:1400/xml/device_description.xml"
                ZoneName="TV Room" Invisible="1"/>
            </ZoneGroupMember></ZoneGroup></ZoneGroups>"#;
        let zgs = parse_zone_group_state(doc).unwrap();
        assert_eq!(zgs.vanished.len(), 0);
        let member = &zgs.groups[0].members[0];
        assert_eq!(member.sw_gen, None);
        assert_eq!(member.boot_seq, None, "absent attribute");
        assert_eq!(member.satellites.len(), 1);
        assert!(member.satellites[0].invisible);
        let bonded: Vec<_> = member.bonded_players().map(|p| p.0.as_str()).collect();
        assert_eq!(
            bonded,
            [
                "RINCON_SB01400",
                "RINCON_SB01400",
                "RINCON_SUB01400",
                "RINCON_SUB01400"
            ]
        );
    }

    #[test]
    fn rejects_unknown_root_and_missing_coordinator() {
        assert!(matches!(
            parse_zone_group_state("<Nope/>"),
            Err(ProtoError::Malformed(_))
        ));
        let no_coord = r#"<ZoneGroups><ZoneGroup ID="x"/></ZoneGroups>"#;
        assert!(matches!(
            parse_zone_group_state(no_coord),
            Err(ProtoError::Malformed(m)) if m.contains("Coordinator")
        ));
    }
}
