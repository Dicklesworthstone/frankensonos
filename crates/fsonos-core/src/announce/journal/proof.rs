//! Fresh physical, group and clip evidence on both sides of a durable claim.

use super::{
    AnnouncementRecovery, CoreError, GroupPlan, HouseholdState, PlayerId, Transport,
    TransportState, recovery, soap,
};
use crate::Room;
use fsonos_proto::didl::equivalent_metadata;
use fsonos_proto::topology::{ZoneGroupState, get_zone_group_state};
use fsonos_types::ZoneGroup;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::net::IpAddr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Identity {
    player: PlayerId,
    host: IpAddr,
    boot: Option<u32>,
    generation: Option<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Physical {
    identities: Vec<Identity>,
}

fn changed(why: &str) -> CoreError {
    CoreError::TargetChanged(format!("announcement recovery ownership changed: {why}"))
}

fn same_ids(a: &[PlayerId], b: &[PlayerId]) -> bool {
    a.len() == b.len() && a.iter().all(|id| b.contains(id))
}

fn unique(ids: &[PlayerId]) -> bool {
    ids.iter().all(|id| !id.0.is_empty()) && ids.iter().collect::<HashSet<_>>().len() == ids.len()
}

fn affected<'a>(household: &'a HouseholdState, plan: &GroupPlan) -> Vec<&'a Room> {
    household
        .rooms
        .iter()
        .filter(|room| plan.zones.contains(&room.coordinator))
        .collect()
}

impl Physical {
    pub(super) fn capture<T: Transport + ?Sized>(
        t: &T,
        household: &HouseholdState,
        plan: &GroupPlan,
    ) -> Result<Self, CoreError> {
        let host = household
            .player(&plan.lead)
            .ok_or_else(|| changed("the lead disappeared"))?
            .ip;
        let topology = get_zone_group_state(t, host)?;
        crate::topology::validate_response(&topology, &plan.lead)?;
        let mut identities = Vec::new();
        for id in affected(household, plan)
            .iter()
            .flat_map(|room| &room.players)
        {
            let known = household
                .player(id)
                .ok_or_else(|| changed("an original bonded player has no address"))?;
            let member = topology
                .groups
                .iter()
                .flat_map(|group| &group.members)
                .flat_map(|member| std::iter::once(member).chain(&member.satellites))
                .find(|member| member.uuid == *id && member.ip() == Some(known.ip))
                .ok_or_else(|| changed("an original player changed address"))?;
            identities.push(Identity {
                player: id.clone(),
                host: known.ip,
                boot: member.boot_seq,
                generation: member.sw_gen,
            });
        }
        let proof = Self { identities };
        proof.verify(&topology, household, plan)?;
        let mut current = household.clone();
        current.apply_topology(&topology);
        for zone in &plan.zones {
            let before = household
                .groups
                .iter()
                .find(|group| group.coordinator == *zone)
                .ok_or_else(|| changed("an original group disappeared"))?;
            if !current.groups.iter().any(|group| {
                group.coordinator == *zone && same_ids(&group.members, &before.members)
            }) {
                return Err(changed("an original group changed during capture"));
            }
        }
        Ok(proof)
    }

    #[allow(clippy::too_many_lines)] // Keep every structural ownership invariant together.
    pub(super) fn valid(&self, record: &AnnouncementRecovery) -> bool {
        let rooms = affected(&record.before, &record.plan);
        let physical: Vec<_> = rooms.iter().flat_map(|room| room.players.clone()).collect();
        let retained: Vec<_> = self
            .identities
            .iter()
            .map(|identity| identity.player.clone())
            .collect();
        let selected: Option<Vec<_>> = record
            .selected
            .iter()
            .map(|id| record.before.rooms.iter().find(|room| room.primary == *id))
            .collect();
        let Some(selected) = selected else {
            return false;
        };
        !self.identities.is_empty()
            && self.identities.len() <= 256
            && unique(&physical)
            && unique(&retained)
            && unique(&record.selected)
            && unique(&record.plan.zones)
            && same_ids(&physical, &retained)
            && self
                .identities
                .iter()
                .map(|identity| identity.host)
                .collect::<HashSet<_>>()
                .len()
                == self.identities.len()
            && self.identities.iter().all(|identity| {
                record
                    .before
                    .player(&identity.player)
                    .is_some_and(|player| player.ip == identity.host)
            })
            && rooms
                .iter()
                .all(|room| room.players.contains(&room.primary) && unique(&room.players))
            && super::super::plan_group(&record.before, &selected).as_ref() == Some(&record.plan)
            && record.snapshots.iter().all(|snapshot| {
                let members: Vec<_> = rooms
                    .iter()
                    .filter(|room| room.coordinator == snapshot.coordinator)
                    .map(|room| room.primary.clone())
                    .collect();
                snapshot.household == record.before.id
                    && record.plan.zones.contains(&snapshot.coordinator)
                    && record
                        .snapshots
                        .iter()
                        .filter(|other| other.coordinator == snapshot.coordinator)
                        .count()
                        == 1
                    && same_ids(&members, &snapshot.members)
                    && snapshot.levels.len() == members.len()
                    && unique(
                        &snapshot
                            .levels
                            .iter()
                            .map(|level| level.player.clone())
                            .collect::<Vec<_>>(),
                    )
                    && snapshot
                        .levels
                        .iter()
                        .all(|level| members.contains(&level.player) && level.volume <= 100)
            })
            && record.effects.groups.len() <= 256
            && record.effects.sources.len() <= 256
            && record.effects.mutes.len() <= 64
            && record.effects.volumes.len() <= 64
            && record
                .effects
                .groups
                .iter()
                .chain(&record.effects.sources)
                .chain(&record.effects.mutes)
                .all(|id| record.selected.contains(id))
            && record
                .effects
                .volumes
                .iter()
                .all(|(id, level)| record.selected.contains(id) && *level <= 100)
            && unique(
                &record
                    .effects
                    .volumes
                    .iter()
                    .map(|(id, _)| id.clone())
                    .collect::<Vec<_>>(),
            )
            && unique(&record.effects.mutes)
            && (!record.established
                || (record.effects.volumes.len() == record.selected.len()
                    && same_ids(&record.effects.mutes, &record.selected)))
            && record.effects.continuations.len() <= record.plan.zones.len()
            && record
                .effects
                .continuations
                .iter()
                .all(|entry| entry.durable_valid(&record.before, &record.selected))
            && equivalent_metadata(&record.clip_metadata, &record.clip_metadata)
    }

    pub(super) fn host(&self, id: &PlayerId) -> Result<IpAddr, CoreError> {
        self.identities
            .iter()
            .find(|identity| identity.player == *id)
            .map(|identity| identity.host)
            .ok_or_else(|| changed("the requested player has no recorded endpoint"))
    }

    pub(super) fn owns_address(&self, host: IpAddr) -> bool {
        self.identities.iter().any(|identity| identity.host == host)
    }

    pub(super) fn boot_known(&self) -> bool {
        self.identities
            .iter()
            .all(|identity| identity.boot.is_some())
    }

    fn verify(
        &self,
        topology: &ZoneGroupState,
        before: &HouseholdState,
        plan: &GroupPlan,
    ) -> Result<(), CoreError> {
        crate::topology::validate_response(topology, &plan.lead)?;
        for identity in &self.identities {
            let member = topology
                .groups
                .iter()
                .flat_map(|group| &group.members)
                .flat_map(|member| std::iter::once(member).chain(&member.satellites))
                .find(|member| member.uuid == identity.player)
                .ok_or_else(|| CoreError::Uncertain(
                    "an original physical player is unavailable; its ownership cannot yet be observed".into()
                ))?;
            if member.ip() != Some(identity.host)
                || member.boot_seq != identity.boot
                || member.sw_gen != identity.generation
            {
                return Err(changed(
                    "a physical player moved, rebooted or changed generation",
                ));
            }
        }
        let mut current = before.clone();
        current.apply_topology(topology);
        if affected(before, plan).iter().any(|room| {
            !current
                .rooms
                .iter()
                .any(|now| super::super::topology::same_room(room, now))
        }) {
            return Err(changed("an original bonded room changed"));
        }
        Ok(())
    }

    pub(super) fn check<T: Transport + ?Sized>(
        &self,
        t: &T,
        record: &AnnouncementRecovery,
    ) -> Result<ZoneGroupState, CoreError> {
        let topology = get_zone_group_state(t, self.host(&record.plan.lead)?)?;
        self.verify(&topology, &record.before, &record.plan)?;
        Ok(topology)
    }
}

impl AnnouncementRecovery {
    /// Exactly the selected temporary group and each unannounced survivor.
    /// A captured observation cannot enlarge this audience after a restart.
    pub(super) fn expected_groups(&self) -> Option<Vec<ZoneGroup>> {
        let rooms = affected(&self.before, &self.plan);
        let mut groups = vec![ZoneGroup {
            coordinator: self.plan.lead.clone(),
            members: rooms
                .iter()
                .filter(|room| self.selected.contains(&room.primary))
                .flat_map(|room| room.players.clone())
                .collect(),
        }];
        if !groups[0].members.contains(&self.plan.lead) {
            return None;
        }
        for zone in &self.plan.zones {
            let members: Vec<_> = rooms
                .iter()
                .filter(|room| room.coordinator == *zone && !self.selected.contains(&room.primary))
                .flat_map(|room| room.players.clone())
                .collect();
            if members.is_empty() {
                continue;
            }
            let coordinator = if members.contains(zone) {
                zone.clone()
            } else {
                self.effects
                    .continuations
                    .iter()
                    .find(|entry| entry.coordinator == *zone)?
                    .survivor()?
                    .clone()
            };
            if !members.contains(&coordinator) {
                return None;
            }
            groups.push(ZoneGroup {
                coordinator,
                members,
            });
        }
        Some(groups)
    }

    fn layout<T: Transport + ?Sized>(&self, t: &T) -> Result<(), CoreError> {
        let topology = self.proof.check(t, self)?;
        let expected = self
            .expected_groups()
            .ok_or_else(|| changed("the completed temporary layout is missing"))?;
        let mut current = self.before.clone();
        current.apply_topology(&topology);
        recovery::check_group(&current, &self.before, &self.plan).map_err(|why| changed(&why))?;
        for group in expected {
            if !current.groups.iter().any(|now| {
                now.coordinator == group.coordinator && same_ids(&now.members, &group.members)
            }) {
                return Err(changed(
                    "the temporary group or an unannounced survivor changed",
                ));
            }
        }
        Ok(())
    }

    pub(super) fn observe<T: Transport + ?Sized>(
        &self,
        t: &T,
    ) -> Result<Option<TransportState>, CoreError> {
        let proof = (|| {
            self.layout(t)?;
            let host = self.proof.host(&self.plan.lead)?;
            let source = || -> Result<(), CoreError> {
                let media = soap::get_media_info(t, host)?;
                if media.uri != self.clip_url
                    || !(media.uri_metadata == self.clip_metadata
                        || equivalent_metadata(&media.uri_metadata, &self.clip_metadata))
                {
                    return Err(changed("the clip source or its native metadata changed"));
                }
                Ok(())
            };
            source()?;
            let state = soap::get_transport_info(t, host)?.state;
            source()?;
            self.layout(t)?;
            let after = soap::get_transport_info(t, host)?.state;
            if state == TransportState::Paused || after == TransportState::Paused {
                return Err(changed("the clip was manually paused"));
            }
            if !matches!(
                state,
                TransportState::Playing | TransportState::Transitioning | TransportState::Stopped
            ) || !matches!(
                after,
                TransportState::Playing | TransportState::Transitioning | TransportState::Stopped
            ) {
                return Err(CoreError::Uncertain(
                    "the interrupted clip's transport is unreadable".into(),
                ));
            }
            source()?;
            Ok(after)
        })();
        match proof {
            Ok(state) => Ok(Some(state)),
            Err(CoreError::TargetChanged(_)) => Ok(None),
            Err(error) => Err(error),
        }
    }
}
