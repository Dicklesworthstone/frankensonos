//! Keep one staged replacement addressed to the original group and source.

use fsonos_proto::soap::{self, AV_TRANSPORT};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::net::IpAddr;

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Destination {
    members: Vec<String>,
    boot_seq: Option<u32>,
    uri: String,
    metadata: String,
}

impl Destination {
    pub(crate) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<Self, ProtoError> {
        let topology = get_zone_group_state(t, host)?;
        let mut groups = topology
            .groups
            .iter()
            .filter(|group| group.coordinator == *coordinator);
        let group = groups.next().ok_or_else(|| {
            ProtoError::Malformed("the replacement target no longer coordinates its group".into())
        })?;
        if groups.next().is_some() {
            return Err(ProtoError::Malformed(
                "the replacement group is ambiguous".into(),
            ));
        }
        // Bonded and satellite players matter, but names, XML ordering,
        // software versions and position-clock progress do not identify a group.
        let mut members: Vec<String> = group
            .members
            .iter()
            .flat_map(fsonos_proto::topology::ZoneMember::bonded_players)
            .map(|player| player.0.clone())
            .collect();
        members.sort();
        members.dedup();
        if !group
            .members
            .iter()
            .any(|member| member.uuid == *coordinator)
        {
            return Err(ProtoError::Malformed(
                "the replacement coordinator is absent from its group".into(),
            ));
        }
        let media = soap::call(
            t,
            host,
            &AV_TRANSPORT,
            "GetMediaInfo",
            &soap::args_xml(&[("InstanceID", "0")]),
        )?;
        // Empty is a valid idle source; absent is not evidence of one.
        let uri = media.require("CurrentURI")?;
        if uri == "NOT_IMPLEMENTED" {
            return Err(ProtoError::Malformed(
                "the replacement source is unavailable".into(),
            ));
        }
        Ok(Self {
            members,
            boot_seq: topology
                .boot_seqs()
                .find_map(|(id, boot)| (id == coordinator).then_some(boot)),
            uri: uri.to_string(),
            metadata: media.get("CurrentURIMetaData").unwrap_or("").to_string(),
        })
    }

    pub(crate) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), ProtoError> {
        if *self != Self::read(t, host, coordinator)? {
            return Err(ProtoError::Malformed(
                "the group membership, boot or source changed while replacing its queue".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn is_queue(&self, coordinator: &PlayerId) -> bool {
        self.uri == format!("x-rincon-queue:{}#0", coordinator.0)
    }
}
