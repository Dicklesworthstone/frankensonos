//! Room resolution: from what a person or agent types to the players a
//! command must address, across both households.
//!
//! Accepted forms, tried in order:
//! 1. a room name, case-insensitive with curly apostrophes folded
//!    (`ada's studio` matches `Ada’s Studio`);
//! 2. a household-qualified room, `Name@S1` / `Name@S2` / `Name@<household id>`,
//!    which disambiguates a name both households use;
//! 3. a player id (`RINCON_…`, case-insensitive), resolving to that player's room.
//!
//! Errors carry what an agent needs to retry: the known rooms, or the
//! qualified candidates for an ambiguous name.

use crate::{CoreError, HouseholdState, Room};
use fsonos_types::{Generation, Player};

/// Normalize a room name for matching: trim, lowercase, and fold curly
/// apostrophes to a straight one (Sonos room names commonly use U+2019).
#[must_use]
pub fn normalize_room(name: &str) -> String {
    name.trim()
        .chars()
        .flat_map(|c| match c {
            '\u{2019}' | '\u{2018}' | '\u{201B}' => '\''.to_lowercase(),
            other => other.to_lowercase(),
        })
        .collect()
}

/// Everything a command for one room needs.
#[derive(Debug, Clone, Copy)]
pub struct ControlTarget<'a> {
    pub household: &'a HouseholdState,
    pub room: &'a Room,
    /// The room's primary player: the address for room-level commands.
    pub player: &'a Player,
    /// The coordinator of the room's group: the address for group-wide
    /// commands (transport, queue, group volume).
    pub coordinator: &'a Player,
}

/// A short, retry-able label per household, index-aligned with `households`:
/// `S1`/`S2` when that generation is unique among them, else the household
/// id, else `#<index>`.
#[must_use]
pub fn household_labels(households: &[HouseholdState]) -> Vec<String> {
    let generations: Vec<_> = households.iter().map(HouseholdState::generation).collect();
    households
        .iter()
        .zip(&generations)
        .enumerate()
        .map(|(i, (h, generation))| match generation {
            Some(g) if generations.iter().filter(|o| *o == generation).count() == 1 => {
                generation_label(*g).to_string()
            }
            _ => {
                h.id.as_ref()
                    .map_or_else(|| format!("#{i}"), |id| id.0.clone())
            }
        })
        .collect()
}

fn generation_label(g: Generation) -> &'static str {
    match g {
        Generation::S1 => "S1",
        Generation::S2 => "S2",
    }
}

/// Every room as `Name@<household label>`, for listings and error messages.
#[must_use]
pub fn known_rooms(households: &[HouseholdState]) -> Vec<String> {
    let labels = household_labels(households);
    households
        .iter()
        .zip(&labels)
        .flat_map(|(h, label)| h.rooms.iter().map(move |r| format!("{}@{label}", r.name)))
        .collect()
}

/// Resolve `query` to a [`ControlTarget`] across `households`.
pub fn resolve_room<'a>(
    households: &'a [HouseholdState],
    query: &str,
) -> Result<ControlTarget<'a>, CoreError> {
    let labels = household_labels(households);
    let mut hits = rooms_named(households, &normalize_room(query), |_| true);
    if hits.is_empty()
        && let Some((name, qualifier)) = query.rsplit_once('@')
    {
        let qualifier = qualifier.trim();
        hits = rooms_named(households, &normalize_room(name), |i| {
            let h = &households[i];
            labels[i].eq_ignore_ascii_case(qualifier)
                || h.id
                    .as_ref()
                    .is_some_and(|id| id.0.eq_ignore_ascii_case(qualifier))
                || h.generation()
                    .is_some_and(|g| generation_label(g).eq_ignore_ascii_case(qualifier))
        });
    }
    if hits.is_empty() {
        let id = query.trim();
        hits = households
            .iter()
            .enumerate()
            .flat_map(|(i, h)| h.rooms.iter().map(move |r| (i, r)))
            .filter(|(_, r)| r.players.iter().any(|p| p.0.eq_ignore_ascii_case(id)))
            .collect();
    }
    match hits.as_slice() {
        [] => Err(CoreError::UnknownRoom {
            name: query.trim().to_string(),
            known: known_rooms(households),
        }),
        [(i, room)] => target(&households[*i], room),
        many => {
            let qualified: Vec<String> = many
                .iter()
                .map(|(i, r)| format!("{}@{}", r.name, labels[*i]))
                .collect();
            // A qualifier only helps when it is unique; otherwise offer the id.
            let candidates = many
                .iter()
                .zip(&qualified)
                .map(|((_, r), q)| {
                    if qualified.iter().filter(|o| *o == q).count() == 1 {
                        q.clone()
                    } else {
                        r.primary.0.clone()
                    }
                })
                .collect();
            Err(CoreError::AmbiguousRoom {
                name: query.trim().to_string(),
                candidates,
            })
        }
    }
}

fn rooms_named<'a>(
    households: &'a [HouseholdState],
    wanted: &str,
    household_ok: impl Fn(usize) -> bool,
) -> Vec<(usize, &'a Room)> {
    households
        .iter()
        .enumerate()
        .filter(|(i, _)| household_ok(*i))
        .flat_map(|(i, h)| h.rooms.iter().map(move |r| (i, r)))
        .filter(|(_, r)| normalize_room(&r.name) == wanted)
        .collect()
}

fn target<'a>(
    household: &'a HouseholdState,
    room: &'a Room,
) -> Result<ControlTarget<'a>, CoreError> {
    let find = |id: &fsonos_types::PlayerId| {
        household
            .player(id)
            .ok_or_else(|| CoreError::UnknownPlayer(id.0.clone()))
    };
    Ok(ControlTarget {
        household,
        room,
        player: find(&room.primary)?,
        coordinator: find(&room.coordinator)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::PlayerId;

    #[test]
    fn normalizes_curly_apostrophe_and_case() {
        assert_eq!(normalize_room("  Ada\u{2019}s Studio "), "ada's studio");
        assert_eq!(normalize_room("ÉTUDE"), "étude");
    }

    #[test]
    fn labels_fall_back_to_id_then_index() {
        let a = HouseholdState {
            id: Some(fsonos_types::HouseholdId("HH_A".into())),
            ..Default::default()
        };
        let b = HouseholdState::default();
        assert_eq!(household_labels(&[a, b]), ["HH_A", "#1"]);
    }

    #[test]
    fn duplicate_names_in_one_household_offer_player_ids() {
        let room = |id: &str| Room {
            name: "Office".into(),
            primary: PlayerId(id.into()),
            players: vec![PlayerId(id.into())],
            missing: Vec::new(),
            coordinator: PlayerId(id.into()),
        };
        let player = |id: &str| Player {
            id: PlayerId(id.into()),
            room_name: "Office".into(),
            ip: "192.0.2.1".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let st = HouseholdState {
            players: vec![player("RINCON_A"), player("RINCON_B")],
            rooms: vec![room("RINCON_A"), room("RINCON_B")],
            ..Default::default()
        };
        let houses = [st];
        match resolve_room(&houses, "office") {
            Err(CoreError::AmbiguousRoom { candidates, .. }) => {
                assert_eq!(candidates, ["RINCON_A", "RINCON_B"]);
            }
            other => panic!("expected AmbiguousRoom, got {other:?}"),
        }
        assert_eq!(
            resolve_room(&houses, "rincon_b").unwrap().player.id.0,
            "RINCON_B"
        );
    }

    #[test]
    fn unknown_room_lists_known_rooms() {
        let err = resolve_room(&[], "Garage").unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown room \"Garage\"; known rooms: (none discovered yet)"
        );
    }
}
