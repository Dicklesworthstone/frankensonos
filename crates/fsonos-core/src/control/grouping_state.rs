//! Bind grouping to coherent household endpoints, then recover an uncertain
//! reply from fresh topology. Only reads repeat after an ambiguous write.

use super::{LEAVE_POLL, LEAVE_WAIT};
use crate::{CoreError, HouseholdState};
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState, ZoneMember, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::net::IpAddr;
use std::time::{Duration, Instant};

type Members = HashSet<PlayerId>;

fn changed(detail: &str) -> CoreError {
    CoreError::TargetChanged(format!("grouping target changed: {detail}"))
}

fn household<'a>(
    households: &'a [HouseholdState],
    player: &PlayerId,
) -> Result<(usize, &'a HouseholdState), CoreError> {
    let mut owners = households
        .iter()
        .enumerate()
        .filter(|(_, house)| house.player(player).is_some());
    let selected = owners
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(player.0.clone()))?;
    if owners.next().is_some() {
        return Err(changed("the player belongs to more than one household"));
    }
    Ok(selected)
}

fn player_at(state: &ZoneGroupState, player: &PlayerId, host: IpAddr) -> bool {
    described(state, player)
        .is_some_and(|member| member.ip() == Some(host) && !member.is_zone_bridge)
}

fn described<'a>(state: &'a ZoneGroupState, player: &PlayerId) -> Option<&'a ZoneMember> {
    state
        .groups
        .iter()
        .flat_map(|group| &group.members)
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .find(|member| member.uuid == *player)
}

/// A topology is household-wide: finding the requested player somewhere in
/// it does not establish who answered at the cached control address.
fn validate(
    households: &[HouseholdState],
    index: usize,
    state: &ZoneGroupState,
    player: &PlayerId,
    host: IpAddr,
) -> Result<(), CoreError> {
    crate::topology::validate(state)?;
    if !player_at(state, player, host) {
        return Err(changed("the requested player no longer owns this address"));
    }
    for (other_index, other) in households.iter().enumerate() {
        if index == other_index {
            continue;
        }
        let foreign =
            |id: &PlayerId| other.player(id).is_some() || other.coordinator_of(id).is_some();
        if state
            .groups
            .iter()
            .flat_map(|group| &group.members)
            .flat_map(|member| std::iter::once(member).chain(&member.satellites))
            .any(|member| {
                foreign(&member.uuid)
                    || other
                        .players
                        .iter()
                        .any(|known| Some(known.ip) == member.ip())
            })
            || state.vanished.iter().any(|member| foreign(&member.uuid))
        {
            return Err(ProtoError::Malformed(
                "grouping topology conflicts with another known household".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// Both endpoints must still belong to the requested players, and both
/// views must name the intended destination as a coordinator. Read the
/// moving room last, immediately before its one-shot join. Retain this
/// current household for any uncertain-reply observation.
pub(super) fn prepare_join<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
    guard: impl Fn(&ZoneGroupState) -> Result<(), CoreError>,
) -> Result<HouseholdState, CoreError> {
    let (index, known) = household(households, member)?;
    let (destination_index, _) = household(households, coordinator)?;
    if index != destination_index {
        return Err(changed("the rooms belong to different households"));
    }
    let host = known.player(member).expect("household found the player").ip;
    let destination = known
        .player(coordinator)
        .expect("the destination belongs to this household")
        .ip;
    let target = get_zone_group_state(t, destination)?;
    validate(households, index, &target, coordinator, destination)?;
    if !crate::topology::coordinator_at(&target, destination, coordinator)?
        || !player_at(&target, member, host)
    {
        return Err(changed(
            "the requested rooms no longer share this destination",
        ));
    }
    guard(&target)?;
    let current = get_zone_group_state(t, host)?;
    validate(households, index, &current, member, host)?;
    guard(&current)?;
    if !crate::topology::coordinator_at(&current, destination, coordinator)? {
        return Err(changed("the destination no longer leads its group"));
    }
    let mut household = known.clone();
    household.apply_topology(&current);
    Ok(household)
}

/// Select a leaving room's heir from coherent current topology, without
/// adopting a new endpoint from the stale address's response.
pub(super) fn prepare_leave<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Result<HouseholdState, CoreError> {
    let (index, known) = household(households, member)?;
    let host = known.player(member).expect("household found the player").ip;
    let current = get_zone_group_state(t, host)?;
    validate(households, index, &current, member, host)?;
    let mut household = known.clone();
    household.apply_topology(&current);
    Ok(household)
}

pub(super) fn uncertain(error: &ProtoError) -> bool {
    matches!(error, ProtoError::Network { .. } | ProtoError::Malformed(_))
}

fn members(group: &ZoneGroupInfo) -> Members {
    group
        .members
        .iter()
        .flat_map(|member| std::iter::once(member).chain(&member.satellites))
        .map(|member| member.uuid.clone())
        .collect()
}

/// Reject duplicate coordinators and members rather than choosing one view.
fn group(state: &ZoneGroupState, coordinator: &PlayerId) -> Option<Members> {
    let mut groups = state
        .groups
        .iter()
        .filter(|g| g.coordinator == *coordinator);
    let selected = groups.next()?;
    if groups.next().is_some() {
        return None;
    }
    let count: usize = selected
        .members
        .iter()
        .map(|m| 1 + m.satellites.len())
        .sum();
    let selected = members(selected);
    if !selected.contains(coordinator)
        || selected.len() != count
        || state
            .groups
            .iter()
            .filter(|g| g.coordinator != *coordinator)
            .any(|g| !members(g).is_disjoint(&selected))
    {
        return None;
    }
    Some(selected)
}

fn room(household: &HouseholdState, player: &PlayerId) -> Option<Members> {
    household
        .rooms
        .iter()
        .find(|room| room.primary == *player && room.players.contains(player))
        .map(|room| room.players.iter().cloned().collect())
}

/// Prefer the receiving/remaining coordinator over a player whose own view
/// can lag after regrouping. Retire a failed peer for this bounded recovery.
fn observe<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    preferred: &[&PlayerId],
    wait: Duration,
    matches: impl Fn(&ZoneGroupState) -> bool,
) -> Option<ZoneGroupState> {
    let mut hosts = Vec::new();
    for id in preferred
        .iter()
        .copied()
        .chain(household.players.iter().map(|p| &p.id))
    {
        if let Some(player) = household.player(id)
            && !hosts.iter().any(|(_, host)| *host == player.ip)
        {
            hosts.push((player.id.clone(), player.ip));
        }
    }
    let started = Instant::now();
    let mut read_any = false;
    loop {
        let mut failed = Vec::new();
        for (responder, host) in &hosts {
            // A blocking SOAP call can exhaust this whole budget. Do not
            // multiply its timeout by every offline player in the household.
            if read_any && started.elapsed() >= wait {
                return None;
            }
            read_any = true;
            match get_zone_group_state(t, *host) {
                Ok(state) => {
                    if crate::topology::validate(&state).is_err()
                        || !player_at(&state, responder, *host)
                    {
                        failed.push(*host);
                    } else if matches(&state) {
                        return Some(state);
                    }
                }
                Err(_) => failed.push(*host),
            }
        }
        hosts.retain(|(_, host)| !failed.contains(host));
        let left = wait.saturating_sub(started.elapsed());
        if hosts.is_empty() || left.is_zero() {
            return None;
        }
        std::thread::sleep(LEAVE_POLL.min(left));
    }
}

pub(super) fn joined<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
    wait: Duration,
) -> bool {
    let Some(household) = households
        .iter()
        .find(|h| h.player(member).is_some() && h.player(coordinator).is_some())
    else {
        return false;
    };
    let Some(expected) = room(household, member) else {
        return false;
    };
    let Some(destination) = household
        .groups
        .iter()
        .find(|g| g.coordinator == *coordinator)
    else {
        return false;
    };
    observe(t, household, &[coordinator, member], wait, |state| {
        group(state, coordinator).is_some_and(|members| {
            expected.is_subset(&members) && destination.members.iter().all(|p| members.contains(p))
        })
    })
    .is_some()
}

pub(super) fn left<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> bool {
    let Some(household) = households.iter().find(|h| h.player(member).is_some()) else {
        return false;
    };
    let Some(expected) = room(household, member) else {
        return false;
    };
    let old_coordinator = household.coordinator_of(member).unwrap_or(member);
    observe(
        t,
        household,
        &[old_coordinator, member],
        LEAVE_WAIT,
        |state| group(state, member).is_some_and(|members| members == expected),
    )
    .is_some()
}

/// A member's self-join can leave its bonded half with the old coordinator,
/// just as a coordinator handoff can. Admit that partial departure only when
/// the original group's complete audience is still accounted for, so the
/// caller can reunite the room without borrowing a player from another group.
pub(super) fn departed<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
) -> Option<ZoneGroupState> {
    let household = households.iter().find(|h| h.player(member).is_some())?;
    let previous = household.coordinator_of(member)?;
    observe(t, household, &[previous, member], LEAVE_WAIT, |state| {
        departure_matches(household, member, previous, state)
    })
}

/// Retain the observed handoff for reuniting the old leader's bonded players.
/// Both acknowledged and uncertain replies need the same actual transition.
pub(super) fn delegated<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    heir: &PlayerId,
) -> Option<ZoneGroupState> {
    let household = households.iter().find(|h| h.player(member).is_some())?;
    observe(t, household, &[heir, member], LEAVE_WAIT, |state| {
        departure_matches(household, member, heir, state)
    })
}

fn departure_matches(
    household: &HouseholdState,
    member: &PlayerId,
    remaining_coordinator: &PlayerId,
    state: &ZoneGroupState,
) -> bool {
    let Some(own) = room(household, member) else {
        return false;
    };
    let Some(left) = group(state, member) else {
        return false;
    };
    if remaining_coordinator == member {
        return left == own;
    }
    let Some(original) = household.groups.iter().find(|g| g.members.contains(member)) else {
        return false;
    };
    let original: Members = original.members.iter().cloned().collect();
    let retained: Members = original.difference(&own).cloned().collect();
    group(state, remaining_coordinator).is_some_and(|remaining| {
        retained.contains(remaining_coordinator)
            && left.is_subset(&own)
            && retained.is_subset(&remaining)
            && left.is_disjoint(&remaining)
            && left.union(&remaining).cloned().collect::<Members>() == original
    })
}

/// Recheck the missing player's own endpoint immediately before its one-shot
/// reunion. A later regroup, reboot, address move or bond edit cannot turn the
/// retained departure into authority over a new room. Automatic reunification
/// is already success and needs no redundant write.
pub(super) fn needs_reunion<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    companion: &PlayerId,
    departed: &ZoneGroupState,
) -> Result<bool, CoreError> {
    let (index, household) = household(households, member)?;
    let host = household
        .player(companion)
        .ok_or_else(|| CoreError::UnknownPlayer(companion.0.clone()))?
        .ip;
    let remaining = departed
        .groups
        .iter()
        .find(|g| members(g).contains(companion))
        .ok_or_else(|| changed("the bonded player's departure was not observed"))?;
    let current = crate::topology::read_at(t, host, companion)?;
    validate(households, index, &current, companion, host)?;
    if !departure_matches(household, member, &remaining.coordinator, &current) {
        return Err(changed(
            "the original groups changed before bonded-room reunion",
        ));
    }
    let original = household
        .groups
        .iter()
        .find(|g| g.members.contains(member))
        .ok_or_else(|| changed("the original group is unavailable"))?;
    for id in &original.members {
        let (Some(before), Some(after)) = (described(departed, id), described(&current, id)) else {
            return Err(changed(
                "a retained player vanished before bonded-room reunion",
            ));
        };
        if before.ip() != after.ip()
            || before
                .boot_seq
                .is_some_and(|boot| after.boot_seq != Some(boot))
            || before.invisible != after.invisible
            || before.is_zone_bridge != after.is_zone_bridge
            || before.channel_map != after.channel_map
            || before.ht_sat_chan_map != after.ht_sat_chan_map
        {
            return Err(changed(
                "a retained player's identity or bond changed before reunion",
            ));
        }
    }
    Ok(!group(&current, member).is_some_and(|own| own.contains(companion)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::{Generation, Player};
    use std::cell::Cell;
    use std::net::IpAddr;

    struct Unreachable(Cell<usize>);

    impl Transport for Unreachable {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            self.0.set(self.0.get() + 1);
            Err(ProtoError::Network {
                target: "synthetic player".into(),
                detail: "unavailable".into(),
            })
        }
    }

    #[test]
    fn an_exhausted_observation_budget_never_starts_another_peer_read() {
        let household = HouseholdState {
            players: (1..=14)
                .map(|n| Player {
                    id: PlayerId(format!("synthetic-{n}")),
                    ip: std::net::Ipv4Addr::new(192, 0, 2, n).into(),
                    room_name: format!("Synthetic {n}"),
                    model: "test".into(),
                    generation: Generation::S2,
                })
                .collect(),
            ..HouseholdState::default()
        };
        let t = Unreachable(Cell::new(0));
        assert!(observe(&t, &household, &[], Duration::ZERO, |_| false).is_none());
        assert_eq!(t.0.get(), 1, "one initial read, no more after the budget");
    }
}
