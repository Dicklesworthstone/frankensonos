//! Zone (group) listings: `GET /zones`, `GET /zones/{room}`, and the
//! `list_zones` tool.
//!
//! A zone is what the Sonos apps show as one playing group: a coordinator and
//! the rooms rendering its audio in sync. Rooms come from the core's topology
//! (a stereo pair or home-theater set is one room); transport state comes from
//! the daemon's live event view, passed in as a lookup.

use fsonos_core::rooms::{household_labels, normalize_room};
use fsonos_core::{HouseholdState, Room};
use fsonos_types::{PlayerId, TransportState};
use serde::{Deserialize, Serialize};

use crate::failure::Failure;
use crate::plan::resolve;

/// One playing group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZoneDto {
    /// The room whose player coordinates the group.
    pub coordinator_room: String,
    /// Every room in the group: the coordinator's first, the rest by name.
    pub members: Vec<String>,
    /// `playing`, `paused`, `stopped`, `transitioning` or `unknown`.
    pub transport_state: String,
    /// The household (`S1`, `S2`, or its id). `Room@<household>` names a room
    /// unambiguously when both households use the same name.
    pub household: String,
    /// Rooms missing a bonded player (e.g. one half of a stereo pair offline).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub degraded: Vec<String>,
}

/// The wire name of a transport state.
#[must_use]
pub fn transport_state_name(state: TransportState) -> &'static str {
    match state {
        TransportState::Playing => "playing",
        TransportState::Paused => "paused",
        TransportState::Stopped => "stopped",
        TransportState::Transitioning => "transitioning",
        TransportState::Unknown => "unknown",
    }
}

/// Every zone across `households`, ordered by household, then coordinator
/// room. `transport` reports a coordinator's last known transport state.
pub fn zone_views(
    households: &[HouseholdState],
    transport: impl Fn(&PlayerId) -> TransportState,
) -> Vec<ZoneDto> {
    let labels = household_labels(households);
    let mut zones: Vec<ZoneDto> = households
        .iter()
        .zip(&labels)
        .flat_map(|(household, label)| {
            let mut coordinators: Vec<&PlayerId> = Vec::new();
            for room in &household.rooms {
                if !coordinators.contains(&&room.coordinator) {
                    coordinators.push(&room.coordinator);
                }
            }
            coordinators
                .into_iter()
                .map(|c| zone_for(household, label, c, &transport))
                .collect::<Vec<_>>()
        })
        .collect();
    zones.sort_by_cached_key(|z| (z.household.clone(), normalize_room(&z.coordinator_room)));
    zones
}

/// The zone that `room` (any form [`fsonos_core::resolve_room`] accepts)
/// currently plays in.
pub fn zone_of(
    households: &[HouseholdState],
    room: &str,
    transport: impl Fn(&PlayerId) -> TransportState,
) -> Result<ZoneDto, Failure> {
    let target = resolve(households, room)?;
    let labels = household_labels(households);
    let index = households
        .iter()
        .position(|h| std::ptr::eq(h, target.household))
        .expect("resolve_room returns a target inside `households`");
    Ok(zone_for(
        target.household,
        &labels[index],
        &target.room.coordinator,
        &transport,
    ))
}

fn zone_for(
    household: &HouseholdState,
    label: &str,
    coordinator: &PlayerId,
    transport: &impl Fn(&PlayerId) -> TransportState,
) -> ZoneDto {
    let mut rooms: Vec<&Room> = household
        .rooms
        .iter()
        .filter(|r| r.coordinator == *coordinator)
        .collect();
    // The coordinator's own room first, the rest by name.
    rooms.sort_by_cached_key(|r| (!r.players.contains(coordinator), normalize_room(&r.name)));
    let coordinator_room = rooms
        .first()
        .filter(|r| r.players.contains(coordinator))
        .map(|r| r.name.clone())
        .or_else(|| household.player(coordinator).map(|p| p.room_name.clone()))
        .unwrap_or_else(|| coordinator.0.clone());
    ZoneDto {
        coordinator_room,
        members: rooms.iter().map(|r| r.name.clone()).collect(),
        transport_state: transport_state_name(transport(coordinator)).to_string(),
        household: label.to_string(),
        degraded: rooms
            .iter()
            .filter(|r| r.is_degraded())
            .map(|r| r.name.clone())
            .collect(),
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    //! Two synthetic households: S1 with `Den` + `Kitchen` grouped (Den
    //! coordinates) and a standalone `Ada’s Studio` pair whose right half is
    //! missing; S2 with its own `Kitchen` and a standalone `Patio`.

    use fsonos_core::{HouseholdState, Room};
    use fsonos_types::{Generation, HouseholdId, Player, PlayerId, ZoneGroup};

    pub fn id(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    fn player(pid: &str, room: &str, generation: Generation) -> Player {
        Player {
            id: id(pid),
            room_name: room.into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation,
        }
    }

    fn room(name: &str, players: &[&str], missing: &[&str], coordinator: &str) -> Room {
        Room {
            name: name.into(),
            primary: id(players[0]),
            players: players.iter().map(|p| id(p)).collect(),
            missing: missing.iter().map(|p| id(p)).collect(),
            coordinator: id(coordinator),
        }
    }

    pub fn households() -> Vec<HouseholdState> {
        use Generation::{S1, S2};
        let s1 = HouseholdState {
            id: Some(HouseholdId("HH_ONE".into())),
            players: vec![
                player("RINCON_DEN", "Den", S1),
                player("RINCON_KIT1", "Kitchen", S1),
                player("RINCON_STU_L", "Ada\u{2019}s Studio", S1),
            ],
            groups: vec![
                ZoneGroup {
                    coordinator: id("RINCON_DEN"),
                    members: vec![id("RINCON_DEN"), id("RINCON_KIT1")],
                },
                ZoneGroup {
                    coordinator: id("RINCON_STU_L"),
                    members: vec![id("RINCON_STU_L")],
                },
            ],
            rooms: vec![
                room("Kitchen", &["RINCON_KIT1"], &[], "RINCON_DEN"),
                room("Den", &["RINCON_DEN"], &[], "RINCON_DEN"),
                room(
                    "Ada\u{2019}s Studio",
                    &["RINCON_STU_L"],
                    &["RINCON_STU_R"],
                    "RINCON_STU_L",
                ),
            ],
        };
        let s2 = HouseholdState {
            id: Some(HouseholdId("HH_TWO".into())),
            players: vec![
                player("RINCON_KIT2", "Kitchen", S2),
                player("RINCON_PATIO", "Patio", S2),
            ],
            groups: vec![
                ZoneGroup {
                    coordinator: id("RINCON_KIT2"),
                    members: vec![id("RINCON_KIT2")],
                },
                ZoneGroup {
                    coordinator: id("RINCON_PATIO"),
                    members: vec![id("RINCON_PATIO")],
                },
            ],
            rooms: vec![
                room("Patio", &["RINCON_PATIO"], &[], "RINCON_PATIO"),
                room("Kitchen", &["RINCON_KIT2"], &[], "RINCON_KIT2"),
            ],
        };
        vec![s1, s2]
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{households, id};
    use super::*;

    fn playing_den(p: &PlayerId) -> TransportState {
        if p.0 == "RINCON_DEN" {
            TransportState::Playing
        } else {
            TransportState::Stopped
        }
    }

    #[test]
    fn lists_one_zone_per_group_in_stable_order() {
        let zones = zone_views(&households(), playing_den);
        let summary: Vec<(&str, &str, Vec<&str>, &str)> = zones
            .iter()
            .map(|z| {
                (
                    z.household.as_str(),
                    z.coordinator_room.as_str(),
                    z.members.iter().map(String::as_str).collect(),
                    z.transport_state.as_str(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                (
                    "S1",
                    "Ada\u{2019}s Studio",
                    vec!["Ada\u{2019}s Studio"],
                    "stopped"
                ),
                ("S1", "Den", vec!["Den", "Kitchen"], "playing"),
                ("S2", "Kitchen", vec!["Kitchen"], "stopped"),
                ("S2", "Patio", vec!["Patio"], "stopped"),
            ]
        );
    }

    #[test]
    fn flags_degraded_rooms() {
        let zones = zone_views(&households(), |_| TransportState::Unknown);
        let studio = zones
            .iter()
            .find(|z| z.coordinator_room.starts_with("Ada"))
            .unwrap();
        assert_eq!(studio.degraded, ["Ada\u{2019}s Studio"]);
        assert_eq!(studio.transport_state, "unknown");
        let json = serde_json::to_value(&zones[1]).unwrap();
        assert!(
            json.get("degraded").is_none(),
            "empty list is omitted: {json}"
        );
    }

    #[test]
    fn zone_of_finds_the_group_a_room_plays_in() {
        let houses = households();
        let z = zone_of(&houses, "kitchen@s1", playing_den).unwrap();
        assert_eq!(
            (z.coordinator_room.as_str(), z.household.as_str()),
            ("Den", "S1")
        );
        let z = zone_of(&houses, "ada's studio", playing_den).unwrap();
        assert_eq!(z.members, ["Ada\u{2019}s Studio"]);
        let z = zone_of(&houses, "rincon_kit2", playing_den).unwrap();
        assert_eq!(z.household, "S2");
    }

    #[test]
    fn zone_of_reports_unknown_and_ambiguous_rooms() {
        let houses = households();
        let unknown = zone_of(&houses, "Garage", playing_den).unwrap_err();
        assert_eq!(unknown.status, 404);
        assert!(unknown.detail.contains("Patio@S2"), "{unknown}");
        let ambiguous = zone_of(&houses, "Kitchen", playing_den).unwrap_err();
        assert_eq!(ambiguous.status, 409);
        assert!(
            ambiguous.detail.contains("Kitchen@S1") && ambiguous.detail.contains("Kitchen@S2"),
            "{ambiguous}"
        );
    }

    #[test]
    fn coordinator_outside_every_room_falls_back_to_its_player() {
        let mut houses = households();
        houses[1].rooms.retain(|r| r.name == "Kitchen");
        houses[1].rooms[0].coordinator = id("RINCON_PATIO");
        let zones = zone_views(&houses, |_| TransportState::Paused);
        let patio = zones.iter().find(|z| z.household == "S2").unwrap();
        assert_eq!(patio.coordinator_room, "Patio");
        assert_eq!(patio.members, ["Kitchen"]);
    }
}
