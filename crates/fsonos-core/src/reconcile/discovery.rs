//! Rediscovery hints outlive the live topology: an absent player is not an
//! active room, but forgetting its address would prevent its return on a
//! multicast-filtered LAN. Nothing here is published as playback or health.

use crate::HouseholdState;

#[derive(Debug, Default)]
pub(super) struct Discovery {
    households: Vec<HouseholdState>,
}

impl Discovery {
    pub(super) fn households(&self) -> &[HouseholdState] {
        &self.households
    }

    pub(super) fn observe(&mut self, fresh: &[HouseholdState]) {
        for house in fresh.iter().filter(|h| !h.players.is_empty()) {
            let index = self.households.iter().position(|old| match (&old.id, &house.id) {
                (Some(a), Some(b)) => a == b,
                _ => house.players.iter().any(|p| old.player(&p.id).is_some()),
            });
            let index = index.unwrap_or_else(|| {
                self.households.push(HouseholdState::default());
                self.households.len() - 1
            });
            // A UUID can move between households, and DHCP can reuse an
            // address for a different UUID. Keep only the latest binding.
            for old in &mut self.households {
                old.players.retain(|p| {
                    !house.players.iter().any(|now| now.id == p.id || now.ip == p.ip)
                });
            }
            let known = &mut self.households[index];
            if house.id.is_some() {
                known.id.clone_from(&house.id);
            }
            // Prefer peers seen most recently. Missing peers remain hints,
            // but can never restore themselves without a fresh response.
            let mut players = house.players.clone();
            players.append(&mut known.players);
            known.players = players;
            self.households.retain(|h| !h.players.is_empty());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::{Generation, HouseholdId, Player, PlayerId, ZoneGroup};
    use std::net::{IpAddr, Ipv4Addr};

    fn house(id: Option<&str>, players: &[(&str, u8)]) -> HouseholdState {
        HouseholdState {
            id: id.map(|id| HouseholdId(id.into())),
            players: players
                .iter()
                .map(|(id, host)| Player {
                    id: PlayerId((*id).into()),
                    room_name: (*id).into(),
                    ip: Ipv4Addr::new(192, 0, 2, *host).into(),
                    model: "test".into(),
                    generation: Generation::S2,
                })
                .collect(),
            ..HouseholdState::default()
        }
    }

    #[test]
    fn missing_players_survive_only_as_discovery_hints() {
        let mut cache = Discovery::default();
        let mut initial = house(Some("one"), &[("a", 1), ("b", 2)]);
        initial.groups.push(ZoneGroup {
            coordinator: PlayerId("a".into()),
            members: vec![PlayerId("a".into()), PlayerId("b".into())],
        });
        cache.observe(&[initial]);
        cache.observe(&[house(Some("one"), &[("b", 2)])]);
        cache.observe(&[HouseholdState::default()]);
        cache.observe(&[]);
        let remembered = &cache.households()[0];
        assert_eq!(remembered.players.len(), 2);
        assert_eq!(remembered.players[0].id.0, "b", "fresh peers first");
        assert!(remembered.groups.is_empty());
        assert!(remembered.rooms.is_empty());
    }

    #[test]
    fn an_address_change_replaces_the_old_hint_instead_of_accumulating_it() {
        let mut cache = Discovery::default();
        cache.observe(&[house(Some("one"), &[("a", 1), ("b", 2)])]);
        cache.observe(&[house(None, &[("a", 10)])]);
        let remembered = &cache.households()[0];
        assert_eq!(remembered.players.len(), 2);
        assert_eq!(
            remembered.players[0].ip,
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10))
        );
        assert_eq!(remembered.id, Some(HouseholdId("one".into())));
    }

    #[test]
    fn disjoint_anonymous_households_of_the_same_generation_stay_separate() {
        let mut cache = Discovery::default();
        cache.observe(&[house(None, &[("a", 1)]), house(None, &[("b", 2)])]);
        cache.observe(&[house(None, &[("b", 20)])]);
        assert_eq!(cache.households().len(), 2);
        assert_eq!(cache.households()[0].players[0].id.0, "a");
        assert_eq!(cache.households()[1].players[0].id.0, "b");
    }

    #[test]
    fn an_explicit_household_move_does_not_relabel_the_players_left_behind() {
        let mut cache = Discovery::default();
        cache.observe(&[house(Some("one"), &[("a", 1), ("b", 2)])]);
        cache.observe(&[house(Some("two"), &[("a", 1)])]);
        assert_eq!(cache.households().len(), 2);
        assert_eq!(cache.households()[0].id, Some(HouseholdId("one".into())));
        assert_eq!(cache.households()[0].players[0].id.0, "b");
        assert_eq!(cache.households()[1].id, Some(HouseholdId("two".into())));
        assert_eq!(cache.households()[1].players[0].id.0, "a");
    }

    #[test]
    fn reused_addresses_do_not_remain_bound_to_an_absent_uuid() {
        let mut cache = Discovery::default();
        cache.observe(&[house(Some("one"), &[("a", 1), ("b", 2)])]);
        cache.observe(&[house(Some("two"), &[("c", 1)])]);
        let players: Vec<_> = cache.households().iter().flat_map(|h| &h.players).collect();
        assert_eq!(players.len(), 2);
        assert!(players.iter().all(|p| p.id.0 != "a"));
        assert!(players.iter().any(|p| p.id.0 == "b"));
        assert!(players.iter().any(|p| p.id.0 == "c"));
    }

    #[test]
    fn repeated_observations_do_not_grow_the_cache() {
        let mut cache = Discovery::default();
        let fresh = vec![house(Some("one"), &[("a", 1), ("b", 2)])];
        for _ in 0..100 {
            cache.observe(&fresh);
        }
        assert_eq!(cache.households().len(), 1);
        assert_eq!(cache.households()[0].players, fresh[0].players);
    }
}
