//! A topology event replaces one whole household. Admit a coherent candidate
//! before changing the live model or retiring its subscriptions. A real move
//! between households is reconciled by discovery, not an old household's lease.

use crate::HouseholdState;
use fsonos_proto::ProtoError;
use fsonos_proto::topology::{ZoneGroupState, parse_zone_group_state};
use fsonos_types::PlayerId;
use std::collections::HashSet;

fn invalid(detail: &str) -> ProtoError {
    ProtoError::Malformed(format!(
        "incoherent topology event: {detail}; retaining the last household until refreshed"
    ))
}

pub(super) fn apply(
    households: &mut [HouseholdState],
    sender: &PlayerId,
    document: &str,
) -> Result<(), ProtoError> {
    let topology = parse_zone_group_state(document)?;
    let mut owners = households
        .iter()
        .enumerate()
        .filter(|(_, household)| household.player(sender).is_some());
    let Some((index, known)) = owners.next() else {
        return Err(invalid("the sender has no known household"));
    };
    if owners.next().is_some() {
        return Err(invalid("the sender belongs to multiple households"));
    }
    validate(&topology)?;
    let mut candidate = known.clone();
    candidate.apply_topology(&topology);
    if candidate.player(sender).is_none() {
        return Err(invalid("the sender is absent or unaddressable"));
    }
    for (other_index, other) in households.iter().enumerate() {
        if other_index == index {
            continue;
        }
        let foreign = |id: &PlayerId| {
            other.player(id).is_some() || other.coordinator_of(id).is_some()
        };
        if candidate.players.iter().any(|player| {
            foreign(&player.id) || other.players.iter().any(|old| old.ip == player.ip)
        }) || topology.vanished.iter().any(|player| foreign(&player.uuid))
        {
            return Err(invalid("the event conflicts with another known household"));
        }
    }
    households[index] = candidate;
    Ok(())
}

fn validate(topology: &ZoneGroupState) -> Result<(), ProtoError> {
    let mut coordinators = HashSet::new();
    let mut players = HashSet::new();
    let mut addresses = HashSet::new();
    for group in &topology.groups {
        if group.coordinator.0.trim().is_empty()
            || !coordinators.insert(&group.coordinator)
            || !group.members.iter().any(|member| member.uuid == group.coordinator)
        {
            return Err(invalid("a group has no unique present coordinator"));
        }
        for member in group
            .members
            .iter()
            .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        {
            if member.uuid.0.trim().is_empty() || !players.insert(&member.uuid) {
                return Err(invalid("a physical player has an empty or repeated identity"));
            }
            let Some(address) = member.ip() else {
                return Err(invalid("a present player has no usable address"));
            };
            if !addresses.insert(address) {
                return Err(invalid("different players share one control address"));
            }
        }
    }
    let mut vanished = HashSet::new();
    for missing in &topology.vanished {
        if missing.uuid.0.trim().is_empty()
            || !vanished.insert(&missing.uuid)
            || players.contains(&missing.uuid)
        {
            return Err(invalid("vanished-player evidence conflicts with the live players"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
