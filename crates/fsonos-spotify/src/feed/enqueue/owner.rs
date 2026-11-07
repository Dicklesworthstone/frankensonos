//! Bind one append batch to its original player, boot, group and source.
//! Runtime evidence is deliberately not part of a persistent FeedSnapshot.

use fsonos_proto::topology::{ZoneGroupState, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport, soap};
use fsonos_types::PlayerId;
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
struct Group {
    id: String,
    members: Vec<String>,
    boot: Option<u32>,
    generation: Option<u8>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Owner {
    host: IpAddr,
    group: Group,
    source: String,
}

fn invalid(message: &str) -> ProtoError {
    ProtoError::Malformed(format!("DJ queue owner changed: {message}"))
}

fn group(
    topology: &ZoneGroupState,
    host: IpAddr,
    coordinator: &PlayerId,
) -> Result<Group, ProtoError> {
    let mut groups = topology
        .groups
        .iter()
        .filter(|group| group.coordinator == *coordinator);
    let found = groups
        .next()
        .ok_or_else(|| invalid("the player is no longer coordinator"))?;
    if groups.next().is_some() {
        return Err(invalid("the queue coordinator is ambiguous"));
    }
    let mut coordinators = found
        .members
        .iter()
        .filter(|member| member.uuid == *coordinator);
    let player = coordinators
        .next()
        .ok_or_else(|| invalid("the coordinator is absent"))?;
    if coordinators.next().is_some() || player.ip() != Some(host) || player.is_zone_bridge {
        return Err(invalid(
            "the coordinator address or identity no longer matches",
        ));
    }
    let mut members: Vec<String> = found
        .members
        .iter()
        .filter(|member| !member.is_zone_bridge)
        .flat_map(fsonos_proto::topology::ZoneMember::bonded_players)
        .map(|id| id.0.clone())
        .collect();
    members.sort_unstable();
    members.dedup();
    Ok(Group {
        id: found.id.clone(),
        members,
        boot: player.boot_seq,
        generation: player.sw_gen,
    })
}

impl Owner {
    pub(super) fn read<T: Transport + ?Sized>(
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, ProtoError> {
        let topology = get_zone_group_state(transport, host)?;
        let group = group(&topology, host, coordinator)?;
        let response = soap::call(
            transport,
            host,
            &soap::AV_TRANSPORT,
            "GetMediaInfo",
            &soap::args_xml(&[("InstanceID", "0")]),
        )?;
        Ok(Self {
            host,
            group,
            // An observed empty source is valid before DJ startup; an absent
            // field is unknown evidence, not an empty source.
            source: response.require("CurrentURI")?.to_owned(),
        })
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        transport: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), ProtoError> {
        if *self == Self::read(transport, host, coordinator)? {
            Ok(())
        } else {
            Err(invalid(
                "the group, boot, generation or active source differs",
            ))
        }
    }

    pub(super) fn reusable(&self) -> bool {
        self.group.boot.is_some() && matches!(self.group.generation, Some(1 | 2))
    }
}

#[cfg(test)]
mod tests;
