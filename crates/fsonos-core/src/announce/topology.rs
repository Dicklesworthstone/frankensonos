//! A household-wide response must identify its witness at the contacted
//! address before it can authorize an announcement or its restoration.

use crate::{CoreError, HouseholdState, Room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{Player, PlayerId};
use std::collections::HashSet;

pub(super) fn read<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    responder: &Player,
) -> Result<HouseholdState, CoreError> {
    let topology = get_zone_group_state(transport, responder.ip)?;
    crate::topology::validate_response(&topology, &responder.id)?;
    if !topology
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .any(|member| member.uuid == responder.id && member.ip() == Some(responder.ip))
    {
        return Err(ProtoError::Malformed(
            "announcement topology does not identify its responder at the contacted address".into(),
        )
        .into());
    }
    let mut current = household.clone();
    current.apply_topology(&topology);
    Ok(current)
}

/// Try every distinct known endpoint at most once. An empty, conflicting or
/// wrongly addressed response cannot hide a healthy household peer.
pub(super) fn refreshed<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
) -> Result<HouseholdState, CoreError> {
    let mut last = CoreError::UnknownHousehold("a household with no players".into());
    let mut tried = HashSet::new();
    for player in &household.players {
        if !tried.insert(player.ip) {
            continue;
        }
        match read(transport, household, player) {
            Ok(current) => return Ok(current),
            Err(error) => last = error,
        }
    }
    Err(last)
}

fn same_ids(left: &[PlayerId], right: &[PlayerId]) -> bool {
    left.len() == right.len() && left.iter().all(|id| right.contains(id))
}

/// Retain the whole bonded room through admission, regrouping and cleanup.
/// A rename or coordinator handoff may be valid; a new stereo half or changed
/// home-theater membership cannot inherit the old room's announcement.
pub(super) fn same_room(before: &Room, current: &Room) -> bool {
    before.primary == current.primary
        && same_ids(&before.players, &current.players)
        && same_ids(&before.missing, &current.missing)
}
