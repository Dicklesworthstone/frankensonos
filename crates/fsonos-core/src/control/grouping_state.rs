//! Recover an uncertain grouping reply from fresh household topology. Only
//! reads repeat; a different layout never authorizes another group write.

use super::{LEAVE_POLL, LEAVE_WAIT};
use crate::HouseholdState;
use fsonos_proto::topology::{ZoneGroupInfo, ZoneGroupState, get_zone_group_state};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashSet;
use std::time::{Duration, Instant};

type Members = HashSet<PlayerId>;

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
) -> bool {
    let mut hosts = Vec::new();
    for id in preferred
        .iter()
        .copied()
        .chain(household.players.iter().map(|p| &p.id))
    {
        if let Some(player) = household.player(id)
            && !hosts.contains(&player.ip)
        {
            hosts.push(player.ip);
        }
    }
    let started = Instant::now();
    let mut read_any = false;
    loop {
        let mut failed = Vec::new();
        for host in &hosts {
            // A blocking SOAP call can exhaust this whole budget. Do not
            // multiply its timeout by every offline player in the household.
            if read_any && started.elapsed() >= wait {
                return false;
            }
            read_any = true;
            match get_zone_group_state(t, *host) {
                Ok(state) if matches(&state) => return true,
                Ok(_) => {}
                Err(_) => failed.push(*host),
            }
        }
        hosts.retain(|host| !failed.contains(host));
        let left = wait.saturating_sub(started.elapsed());
        if hosts.is_empty() || left.is_zero() {
            return false;
        }
        std::thread::sleep(LEAVE_POLL.min(left));
    }
}

pub(super) fn joined<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    coordinator: &PlayerId,
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
    observe(t, household, &[coordinator, member], LEAVE_WAIT, |state| {
        group(state, coordinator).is_some_and(|members| {
            expected.is_subset(&members) && destination.members.iter().all(|p| members.contains(p))
        })
    })
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
}

pub(super) fn delegated<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    heir: &PlayerId,
) -> bool {
    let Some(household) = households.iter().find(|h| h.player(member).is_some()) else {
        return false;
    };
    let Some(original) = household.groups.iter().find(|g| g.coordinator == *member) else {
        return false;
    };
    let original: Members = original.members.iter().cloned().collect();
    let Some(own) = room(household, member) else {
        return false;
    };
    let retained: Members = original.difference(&own).cloned().collect();
    if !retained.contains(heir) {
        return false;
    }
    observe(t, household, &[heir, member], LEAVE_WAIT, |state| {
        let (Some(left), Some(remaining)) = (group(state, member), group(state, heir)) else {
            return false;
        };
        // S1 can leave the old leader's hidden half with the heir. It is
        // reunited by the existing leave path, then the whole room is checked.
        left.is_subset(&own)
            && retained.is_subset(&remaining)
            && left.is_disjoint(&remaining)
            && left.union(&remaining).cloned().collect::<Members>() == original
    })
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
        assert!(!observe(&t, &household, &[], Duration::ZERO, |_| false));
        assert_eq!(t.0.get(), 1, "one initial read, no more after the budget");
    }
}
