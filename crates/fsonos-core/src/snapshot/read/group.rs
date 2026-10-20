//! Capture the requested coordinator's actual group, including bonded players.
//! Unrelated groups and XML member ordering do not invalidate its snapshot.

use super::{changed, ip};
use crate::{CoreError, HouseholdState};
use fsonos_proto::Transport;
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState, ZoneMember, get_zone_group_state};
use fsonos_types::PlayerId;
use std::net::IpAddr;

pub(super) struct Group(ZoneGroupInfo);

impl Group {
    pub(super) fn begin<T: Transport + ?Sized>(
        t: &T,
        known: &HouseholdState,
        coordinator: &PlayerId,
    ) -> Result<(HouseholdState, Self), CoreError> {
        let topology = get_zone_group_state(t, ip(known, coordinator)?)?;
        let group = canonical(&topology, coordinator).ok_or_else(changed)?;
        let mut fresh = known.clone();
        fresh.apply_topology(&topology);
        // A missing/inaccessible renderer is not a reason to reuse cached
        // membership or silently capture a different coordinator's music.
        ip(&fresh, coordinator)?;
        if !fresh.groups.iter().any(|g| g.coordinator == *coordinator) {
            return Err(changed());
        }
        Ok((fresh, Self(group)))
    }

    pub(super) fn unchanged<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
    ) -> Result<bool, CoreError> {
        let topology = get_zone_group_state(t, host)?;
        Ok(canonical(&topology, &self.0.coordinator).as_ref() == Some(&self.0))
    }
}

fn canonical(topology: &ZoneGroupState, coordinator: &PlayerId) -> Option<ZoneGroupInfo> {
    let mut matches = topology
        .groups
        .iter()
        .filter(|g| g.coordinator == *coordinator);
    let mut group = matches.next()?.clone();
    if matches.next().is_some() {
        return None;
    }
    group.members.sort_by(|a, b| a.uuid.0.cmp(&b.uuid.0));
    for member in &mut group.members {
        normalize(member);
    }
    Some(group)
}

fn normalize(member: &mut ZoneMember) {
    member
        .channel_map
        .sort_by(|a, b| (&a.player.0, &a.channels).cmp(&(&b.player.0, &b.channels)));
    member
        .ht_sat_chan_map
        .sort_by(|a, b| (&a.player.0, &a.channels).cmp(&(&b.player.0, &b.channels)));
    member.satellites.sort_by(|a, b| a.uuid.0.cmp(&b.uuid.0));
    for satellite in &mut member.satellites {
        normalize(satellite);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::topology::parse_zone_group_state;

    fn topology() -> ZoneGroupState {
        parse_zone_group_state(
            r#"<ZoneGroupState><ZoneGroups>
            <ZoneGroup Coordinator="RINCON_TEST_A" ID="synthetic-group">
              <ZoneGroupMember UUID="RINCON_TEST_A" ZoneName="A" BootSeq="1"
                Location="http://192.0.2.10:1400/xml/device_description.xml"
                ChannelMapSet="RINCON_TEST_A:LF;RINCON_TEST_B:RF"/>
              <ZoneGroupMember UUID="RINCON_TEST_B" ZoneName="A" Invisible="1"
                Location="http://192.0.2.11:1400/xml/device_description.xml"
                ChannelMapSet="RINCON_TEST_A:LF;RINCON_TEST_B:RF"/>
            </ZoneGroup></ZoneGroups></ZoneGroupState>"#,
        )
        .unwrap()
    }

    #[test]
    fn xml_member_and_channel_map_order_do_not_change_group_identity() {
        let original = topology();
        let mut reordered = original.clone();
        reordered.groups[0].members.reverse();
        for member in &mut reordered.groups[0].members {
            member.channel_map.reverse();
        }
        let coordinator = PlayerId("RINCON_TEST_A".into());
        assert_eq!(
            canonical(&original, &coordinator),
            canonical(&reordered, &coordinator)
        );
    }

    #[test]
    fn reboot_and_bond_changes_are_not_hidden_by_canonicalization() {
        let original = topology();
        let coordinator = PlayerId("RINCON_TEST_A".into());
        let mut rebooted = original.clone();
        rebooted.groups[0].members[0].boot_seq = Some(2);
        assert_ne!(
            canonical(&original, &coordinator),
            canonical(&rebooted, &coordinator)
        );
        let mut unpaired = original.clone();
        unpaired.groups[0].members[0].channel_map.clear();
        assert_ne!(
            canonical(&original, &coordinator),
            canonical(&unpaired, &coordinator)
        );
        let mut duplicate = original.clone();
        duplicate.groups.push(original.groups[0].clone());
        assert!(canonical(&duplicate, &coordinator).is_none());
    }
}
