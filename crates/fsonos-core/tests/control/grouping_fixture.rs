//! Move one self-closing member in a synthetic SOAP topology reply.

use fsonos_proto::soap::{ZONE_GROUP_TOPOLOGY, args_xml, envelope, parse_response};
use fsonos_types::PlayerId;

pub fn relocate(reply: &str, player: &PlayerId, coordinator: &PlayerId) -> String {
    let response = parse_response(reply, "GetZoneGroupState").unwrap();
    let mut document = response.require("ZoneGroupState").unwrap().to_string();
    let at = document.find(&format!("UUID=\"{}\"", player.0)).unwrap();
    let start = document[..at].rfind("<ZoneGroupMember ").unwrap();
    let end = start + document[start..].find("/>").unwrap() + 2;
    let member = document[start..end].to_string();
    document.replace_range(start..end, "");
    let group = document
        .find(&format!("Coordinator=\"{}\"", coordinator.0))
        .unwrap();
    let insert = group + document[group..].find("</ZoneGroup>").unwrap();
    document.insert_str(insert, &member);
    envelope(
        &ZONE_GROUP_TOPOLOGY,
        "GetZoneGroupStateResponse",
        &args_xml(&[("ZoneGroupState", &document)]),
    )
}
