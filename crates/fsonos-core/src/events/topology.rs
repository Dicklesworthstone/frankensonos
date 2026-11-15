//! A topology event replaces one whole household. Admit a coherent candidate
//! before changing the live model or retiring its subscriptions. A real move
//! between households is reconciled by discovery, not an old household's lease.

use crate::HouseholdState;
use fsonos_proto::ProtoError;
use fsonos_proto::topology::parse_zone_group_state;
use fsonos_types::PlayerId;

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
    crate::topology::validate_response(&topology, sender)?;
    let mut candidate = known.clone();
    candidate.apply_topology(&topology);
    if candidate.player(sender).is_none() {
        return Err(invalid("the sender is absent or unaddressable"));
    }
    for (other_index, other) in households.iter().enumerate() {
        if other_index == index {
            continue;
        }
        let foreign =
            |id: &PlayerId| other.player(id).is_some() || other.coordinator_of(id).is_some();
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

#[cfg(test)]
mod tests;
