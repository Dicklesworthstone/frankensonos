//! Startup addresses have no UUID yet. Keep probing each unresolved hint
//! until live topology observes it; thereafter the reconciler owns recovery.

use super::LiveConfig;
use crate::HouseholdState;
use std::net::IpAddr;

pub(super) fn seeds(explicit: &[IpAddr], pending: &[IpAddr]) -> Vec<IpAddr> {
    unique(explicit.iter().chain(pending).copied())
}

pub(super) fn observed(pending: &mut Vec<IpAddr>, households: &[HouseholdState]) {
    pending.retain(|ip| {
        !households
            .iter()
            .any(|h| h.players.iter().any(|p| p.ip == *ip))
    });
}

pub(super) fn listener_candidates(
    households: &[HouseholdState],
    config: &LiveConfig,
) -> Vec<IpAddr> {
    unique(
        households
            .iter()
            .flat_map(|h| h.players.iter().map(|p| p.ip))
            .chain(config.seeds.iter().copied())
            .chain(config.remembered.iter().copied()),
    )
}

fn unique(addresses: impl Iterator<Item = IpAddr>) -> Vec<IpAddr> {
    let mut out = Vec::new();
    for ip in addresses {
        if !out.contains(&ip) {
            out.push(ip);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::{Generation, Player, PlayerId};
    use std::net::Ipv4Addr;

    fn ip(host: u8) -> IpAddr {
        Ipv4Addr::new(192, 0, 2, host).into()
    }

    fn house(hosts: &[u8]) -> HouseholdState {
        HouseholdState {
            players: hosts
                .iter()
                .map(|host| Player {
                    id: PlayerId(format!("test-{host}")),
                    room_name: format!("Room {host}"),
                    ip: ip(*host),
                    model: "test".into(),
                    generation: Generation::S2,
                })
                .collect(),
            ..HouseholdState::default()
        }
    }

    #[test]
    fn cached_addresses_can_bootstrap_without_any_explicit_seed() {
        let mut config = LiveConfig::new(Vec::new());
        config.remembered = vec![ip(1), ip(2), ip(1)];
        assert_eq!(listener_candidates(&[], &config), [ip(1), ip(2)]);
    }

    #[test]
    fn listener_candidates_prefer_live_then_explicit_then_cached_addresses() {
        let mut config = LiveConfig::new(vec![ip(2), ip(3)]);
        config.remembered = vec![ip(3), ip(4), ip(1)];
        assert_eq!(
            listener_candidates(&[house(&[1, 2])], &config),
            [ip(1), ip(2), ip(3), ip(4)]
        );
    }

    #[test]
    fn a_partial_survey_does_not_forget_unresolved_bootstrap_addresses() {
        let mut pending = vec![ip(1), ip(2), ip(3)];
        observed(&mut pending, &[house(&[1])]);
        assert_eq!(pending, [ip(2), ip(3)]);
        observed(&mut pending, &[]);
        observed(&mut pending, &[house(&[1])]);
        assert_eq!(pending, [ip(2), ip(3)]);
        observed(&mut pending, &[house(&[3])]);
        assert_eq!(pending, [ip(2)]);
    }

    #[test]
    fn discovered_addresses_do_not_become_unconditional_probes_again() {
        let mut pending = vec![ip(1), ip(2)];
        observed(&mut pending, &[house(&[1, 2])]);
        observed(&mut pending, &[]);
        assert!(pending.is_empty());
        assert!(seeds(&[], &pending).is_empty());
    }

    #[test]
    fn explicit_seeds_remain_requested_and_are_deduplicated() {
        let explicit = vec![ip(1), ip(2), ip(1)];
        let mut pending = vec![ip(2), ip(3)];
        assert_eq!(seeds(&explicit, &pending), [ip(1), ip(2), ip(3)]);
        observed(&mut pending, &[house(&[2, 3])]);
        assert_eq!(seeds(&explicit, &pending), [ip(1), ip(2)]);
    }
}
