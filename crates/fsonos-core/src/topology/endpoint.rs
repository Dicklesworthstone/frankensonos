//! Repair a stale advertised address using identity read from that endpoint.
//! The ordinary topology proof stays sufficient. A mismatch adds one fixed-
//! address description GET, bracketed by matching complete topology reads.

use fsonos_proto::description::parse_device_description;
use fsonos_proto::ssdp::description_url;
use fsonos_proto::topology::{ZoneGroupState, ZoneMember, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::iter;
use std::net::IpAddr;

fn validate(topology: &ZoneGroupState) -> Result<(), ProtoError> {
    super::validate(topology)?;
    // Endpoint and room proofs support direct satellites. A deeper child must
    // not hide a conflicting identity/address from the ordinary fast path.
    if topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| &member.satellites)
        .any(|satellite| !satellite.satellites.is_empty())
    {
        return Err(ProtoError::Malformed(
            "endpoint topology contains unsupported nested satellites".into(),
        ));
    }
    Ok(())
}

fn members(topology: &ZoneGroupState) -> impl Iterator<Item = &ZoneMember> {
    topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| iter::once(member).chain(&member.satellites))
}

fn corrected(
    topology: &ZoneGroupState,
    host: IpAddr,
    expected: &PlayerId,
) -> Option<ZoneGroupState> {
    // Do not borrow an endpoint another member claims, even if its description
    // disagrees. Every member, including bridges and satellites, participates.
    if members(topology).any(|member| member.ip() == Some(host) && member.uuid != *expected)
        || !members(topology).any(|member| member.uuid == *expected && !member.is_zone_bridge)
    {
        return None;
    }
    let mut corrected = topology.clone();
    let location = description_url(host);
    for group in &mut corrected.groups {
        for member in &mut group.members {
            if member.uuid == *expected {
                member.location = location;
                return Some(corrected);
            }
            for satellite in &mut member.satellites {
                if satellite.uuid == *expected {
                    satellite.location = location;
                    return Some(corrected);
                }
            }
        }
    }
    None
}

/// Read this endpoint's topology, repairing only the requested player's stale
/// Location after independent identity proof. Callers still enforce their
/// player/coordinator, membership, source, queue and boot requirements.
///
/// A missing player, bridge, claimed address or different described UDN leaves
/// the original topology unchanged so the caller's identity check rejects it.
/// Unavailable or ambiguous evidence remains an error. No discovery is run,
/// no URI from the reply is followed, and no proof is reused for a later read.
pub(crate) fn read_at<T: Transport + ?Sized>(
    transport: &T,
    host: IpAddr,
    expected: &PlayerId,
) -> Result<ZoneGroupState, ProtoError> {
    let original = get_zone_group_state(transport, host)?;
    validate(&original)?;
    if members(&original).any(|member| {
        member.uuid == *expected && member.ip() == Some(host) && !member.is_zone_bridge
    }) {
        return Ok(original);
    }
    let Some(before) = corrected(&original, host, expected) else {
        return Ok(original);
    };
    let advertised = members(&original)
        .find(|member| member.uuid == *expected)
        .and_then(ZoneMember::ip);
    // Transport::http_get is endpoint-bound: redirects cannot lend another
    // player's description to this address. Read the fixed Sonos path only.
    let description = parse_device_description(&transport.http_get(&description_url(host))?)?;
    if description.udn != *expected || !description.is_renderer() {
        return Ok(original);
    }
    let observed = get_zone_group_state(transport, host)?;
    validate(&observed)?;
    if members(&observed).any(|member| {
        member.uuid == *expected && member.ip() != advertised && member.ip() != Some(host)
    }) {
        return Err(ProtoError::Malformed(
            "the player's advertised address changed during endpoint verification".into(),
        ));
    }
    let Some(after) = corrected(&observed, host, expected) else {
        return Ok(observed);
    };
    validate(&after)?;
    // The Location may catch up while the description is read. Nothing else
    // may change: a fresh identity cannot authorize a changed audience or boot.
    if before != after {
        return Err(ProtoError::Malformed(
            "topology changed while verifying the player's control address".into(),
        ));
    }
    Ok(after)
}
