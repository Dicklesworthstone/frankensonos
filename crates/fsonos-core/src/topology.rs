//! Topology: folding a ZoneGroupTopology snapshot into [`HouseholdState`].
//!
//! The snapshot names groups and physical players; people and agents name
//! *rooms*. A room is one player, a stereo pair (`ChannelMapSet`), or a
//! home-theater set (`HTSatChanMapSet` + satellites). Commands for a room go
//! to its primary (the visible member); group-wide commands go to the group
//! coordinator. Zone bridges cannot render audio and are left out entirely.

use crate::{HouseholdState, inventory};
use fsonos_proto::topology::{ZoneGroupState, ZoneMember};
use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};
use std::iter;

/// A logical room ("zone" in the Sonos apps), addressed by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub name: String,
    /// The player room-level commands address: the visible member of a pair
    /// or set. When every visible member is offline it falls back to the
    /// group coordinator, if the coordinator is in this room.
    pub primary: PlayerId,
    /// Every present player bonded into the room, primary first.
    pub players: Vec<PlayerId>,
    /// Players the room's channel maps bond in that the topology does not
    /// list (offline or vanished).
    pub missing: Vec<PlayerId>,
    /// Coordinator of the group the room currently plays in.
    pub coordinator: PlayerId,
}

impl Room {
    /// Whether some bonded player is missing (e.g. one half of a pair).
    #[must_use]
    pub fn is_degraded(&self) -> bool {
        !self.missing.is_empty()
    }

    /// Whether this room's own players coordinate its group.
    #[must_use]
    pub fn is_group_coordinator(&self) -> bool {
        self.players.contains(&self.coordinator)
    }
}

impl HouseholdState {
    /// Replace groups, rooms, and players with those in `zgs`. Model and
    /// generation learned earlier (from device descriptions) carry over for
    /// players that are still present; players no longer listed are dropped.
    /// A player whose `Location` has no usable IP stays in its group and room
    /// but not in [`Self::players`], so control cannot be addressed to it.
    pub fn apply_topology(&mut self, zgs: &ZoneGroupState) {
        let previous = std::mem::take(&mut self.players);
        self.groups.clear();
        self.rooms.clear();
        for group in &zgs.groups {
            let members: Vec<&ZoneMember> =
                group.members.iter().filter(|m| !m.is_zone_bridge).collect();
            if members.is_empty() {
                continue;
            }
            let physical: Vec<&ZoneMember> = members
                .iter()
                .flat_map(|m| iter::once(*m).chain(&m.satellites))
                .collect();
            self.groups.push(ZoneGroup {
                coordinator: group.coordinator.clone(),
                members: physical.iter().map(|m| m.uuid.clone()).collect(),
            });
            self.rooms.extend(
                bonded_sets(&members)
                    .iter()
                    .map(|set| room_of(&group.coordinator, set)),
            );
            self.players
                .extend(physical.iter().filter_map(|m| player_of(m, &previous)));
        }
    }
}

/// Partition a group's members into rooms: two members share a room when
/// either one's channel maps name the other. Sonos writes the same map on
/// every bonded member, so one pass suffices.
fn bonded_sets<'a>(members: &[&'a ZoneMember]) -> Vec<Vec<&'a ZoneMember>> {
    let mut sets: Vec<Vec<&ZoneMember>> = Vec::new();
    for &m in members {
        let bonded = |o: &ZoneMember| {
            o.bonded_players().any(|p| *p == m.uuid) || m.bonded_players().any(|p| *p == o.uuid)
        };
        match sets.iter_mut().find(|set| set.iter().any(|o| bonded(o))) {
            Some(set) => set.push(m),
            None => sets.push(vec![m]),
        }
    }
    sets
}

fn room_of(coordinator: &PlayerId, set: &[&ZoneMember]) -> Room {
    let primary = set
        .iter()
        .find(|m| !m.invisible)
        .or_else(|| set.iter().find(|m| m.uuid == *coordinator))
        .unwrap_or(&set[0]);
    let mut players = vec![primary.uuid.clone()];
    for m in set {
        for p in iter::once(&m.uuid).chain(m.satellites.iter().map(|s| &s.uuid)) {
            if !players.contains(p) {
                players.push(p.clone());
            }
        }
    }
    let mut missing = Vec::new();
    for p in set.iter().flat_map(|m| m.bonded_players()) {
        if !players.contains(p) && !missing.contains(p) {
            missing.push(p.clone());
        }
    }
    Room {
        name: primary.zone_name.clone(),
        primary: primary.uuid.clone(),
        players,
        missing,
        coordinator: coordinator.clone(),
    }
}

fn player_of(m: &ZoneMember, previous: &[Player]) -> Option<Player> {
    let known = previous.iter().find(|p| p.id == m.uuid);
    Some(Player {
        id: m.uuid.clone(),
        room_name: m.zone_name.clone(),
        ip: m.ip()?,
        // The topology does not carry the model; it stays empty until the
        // device description is applied (see `apply_description`).
        model: known.map(|p| p.model.clone()).unwrap_or_default(),
        generation: m
            .sw_gen
            .and_then(inventory::generation_for_sw_gen)
            .or(known.map(|p| p.generation))
            .unwrap_or(Generation::S2),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_proto::topology::parse_zone_group_state;

    fn pid(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    #[test]
    fn home_theater_set_is_one_room_and_bridges_are_dropped() {
        let zgs = parse_zone_group_state(
            r#"<ZoneGroupState><ZoneGroups>
            <ZoneGroup Coordinator="RINCON_SB" ID="RINCON_SB:1">
              <ZoneGroupMember UUID="RINCON_SB" Location="http://192.0.2.5:1400/x" ZoneName="TV Room"
                SWGen="2" HTSatChanMapSet="RINCON_SB:LF,RF;RINCON_SUB:SW;RINCON_RR:RR">
                <Satellite UUID="RINCON_SUB" Location="http://192.0.2.6:1400/x" ZoneName="TV Room" Invisible="1"/>
              </ZoneGroupMember>
            </ZoneGroup>
            <ZoneGroup Coordinator="RINCON_BR" ID="RINCON_BR:0">
              <ZoneGroupMember UUID="RINCON_BR" Location="http://192.0.2.7:1400/x" ZoneName="Bridge"
                Invisible="1" IsZoneBridge="1"/>
            </ZoneGroup></ZoneGroups></ZoneGroupState>"#,
        )
        .unwrap();
        let mut st = HouseholdState::default();
        st.apply_topology(&zgs);
        assert_eq!(st.groups.len(), 1);
        assert_eq!(st.groups[0].members, [pid("RINCON_SB"), pid("RINCON_SUB")]);
        assert_eq!(st.rooms.len(), 1);
        let room = &st.rooms[0];
        assert_eq!(room.name, "TV Room");
        assert_eq!(room.primary, pid("RINCON_SB"));
        assert_eq!(room.players, [pid("RINCON_SB"), pid("RINCON_SUB")]);
        assert_eq!(room.missing, [pid("RINCON_RR")]);
        assert!(room.is_degraded() && room.is_group_coordinator());
        assert_eq!(st.players.len(), 2);
        assert!(st.players.iter().all(|p| p.generation == Generation::S2));
    }

    #[test]
    fn reapplying_keeps_learned_models_and_drops_departed_players() {
        let two = |second: &str| {
            parse_zone_group_state(&format!(
                r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_A" ID="g">
                <ZoneGroupMember UUID="RINCON_A" Location="http://192.0.2.1:1400/x" ZoneName="A" SWGen="1"/>
                {second}</ZoneGroup></ZoneGroups>"#
            ))
            .unwrap()
        };
        let mut st = HouseholdState::default();
        st.apply_topology(&two(
            r#"<ZoneGroupMember UUID="RINCON_B" Location="http://192.0.2.2:1400/x" ZoneName="B"/>"#,
        ));
        assert_eq!(st.rooms.len(), 2);
        st.players[0].model = "Sonos Play:5".into();
        st.apply_topology(&two(""));
        assert_eq!(st.players.len(), 1);
        assert_eq!(st.players[0].model, "Sonos Play:5");
        assert_eq!(st.players[0].generation, Generation::S1);
        assert_eq!(st.coordinator_of(&pid("RINCON_B")), None);
    }

    #[test]
    fn player_without_usable_ip_stays_in_its_room_only() {
        let zgs = parse_zone_group_state(
            r#"<ZoneGroups><ZoneGroup Coordinator="RINCON_A" ID="g">
            <ZoneGroupMember UUID="RINCON_A" Location="garbage" ZoneName="A"/>
            </ZoneGroup></ZoneGroups>"#,
        )
        .unwrap();
        let mut st = HouseholdState::default();
        st.apply_topology(&zgs);
        assert_eq!(st.rooms.len(), 1);
        assert_eq!(st.players.len(), 0);
        assert_eq!(st.coordinator_of(&pid("RINCON_A")), Some(&pid("RINCON_A")));
    }
}
