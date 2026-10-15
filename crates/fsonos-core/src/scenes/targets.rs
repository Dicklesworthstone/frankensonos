//! Scene room names share the interactive resolver's exact matching rules,
//! but never its prefixes or dynamic sets. Qualifiers survive persistence.

use crate::rooms::{known_rooms, named_room, normalize_room};
use crate::{CoreError, HouseholdState, Room};
use fsonos_types::Generation;

/// Canonicalize a scene's explicit room reference. A plain unique name
/// stays readable; a qualified name stays scoped, preferring the household
/// id to a generation label that could become ambiguous after discovery.
/// Missing or ambiguous rooms fail before a definition is saved.
pub fn canonical_room_name(households: &[HouseholdState], asked: &str) -> Result<String, CoreError> {
    let (i, room) = named_room(households, asked)?.ok_or_else(|| CoreError::UnknownRoom {
        name: asked.trim().to_string(),
        known: known_rooms(households),
    })?;
    if normalize_room(asked) == normalize_room(&room.name) {
        return captured_name(households, i, room);
    }
    qualified_name(households, i, room)
}

/// Keep existing unique-name captures readable, but never overwrite another
/// household's identically named room in a volume/mute map.
pub(super) fn captured_name(
    households: &[HouseholdState],
    i: usize,
    room: &Room,
) -> Result<String, CoreError> {
    let normalized = normalize_room(&room.name);
    let unique = households
        .iter()
        .flat_map(|h| &h.rooms)
        .filter(|r| normalize_room(&r.name) == normalized)
        .count() == 1;
    if unique && refers_to(households, &room.name, i, room) {
        return Ok(room.name.clone());
    }
    qualified_name(households, i, room)
}

fn qualified_name(
    households: &[HouseholdState],
    i: usize,
    room: &Room,
) -> Result<String, CoreError> {
    let mut candidates = Vec::new();
    if let Some(id) = &households[i].id {
        candidates.push(format!("{}@{}", room.name, id.0));
    }
    if let Some(generation) = households[i].generation() {
        let label = match generation {
            Generation::S1 => "S1",
            Generation::S2 => "S2",
        };
        candidates.push(format!("{}@{label}", room.name));
    }
    // A same-household duplicate (or a household with no stable label) can
    // still be named by player id. Never persist an order-dependent #index.
    candidates.push(room.primary.0.clone());
    for candidate in &candidates {
        // A literal room name can itself contain '@' or look like a player
        // id. Prove the emitted reference resolves back to this exact room.
        if refers_to(households, candidate, i, room) {
            return Ok(candidate.clone());
        }
    }
    Err(CoreError::AmbiguousRoom {
        name: room.name.clone(),
        candidates,
    })
}

fn refers_to(households: &[HouseholdState], name: &str, i: usize, room: &Room) -> bool {
    named_room(households, name)
        .ok()
        .flatten()
        .is_some_and(|(j, r)| i == j && r.primary == room.primary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::{
        Scene, SceneError, SceneGroup, SceneOp, SceneSource, check, plan_apply, zones,
    };
    use fsonos_types::{HouseholdId, Player, PlayerId, ZoneGroup};
    use std::collections::BTreeMap;

    fn household(id: &str, generation: Generation, names: &[(&str, &str)]) -> HouseholdState {
        HouseholdState {
            id: Some(HouseholdId(id.into())),
            players: names
                .iter()
                .map(|(id, name)| Player {
                    id: PlayerId((*id).into()),
                    room_name: (*name).into(),
                    ip: "192.0.2.1".parse().unwrap(),
                    model: String::new(),
                    generation,
                })
                .collect(),
            groups: names
                .iter()
                .map(|(id, _)| ZoneGroup {
                    coordinator: PlayerId((*id).into()),
                    members: vec![PlayerId((*id).into())],
                })
                .collect(),
            rooms: names
                .iter()
                .map(|(id, name)| Room {
                    name: (*name).into(),
                    primary: PlayerId((*id).into()),
                    players: vec![PlayerId((*id).into())],
                    missing: Vec::new(),
                    coordinator: PlayerId((*id).into()),
                })
                .collect(),
        }
    }

    fn houses() -> Vec<HouseholdState> {
        vec![
            household(
                "HH_OLD",
                Generation::S1,
                &[("OLD_K", "Kitchen"), ("OLD_D", "Den")],
            ),
            household(
                "HH_NEW",
                Generation::S2,
                &[("NEW_K", "Kitchen"), ("NEW_D", "Den")],
            ),
        ]
    }

    fn group(lead: &str, member: &str) -> SceneGroup {
        SceneGroup {
            coordinator: lead.into(),
            members: vec![member.into()],
            source: SceneSource::Keep,
            playing: None,
        }
    }

    fn scene() -> Scene {
        Scene {
            name: "Both houses".into(),
            groups: vec![
                group("Kitchen@S1", "Den@S1"),
                group("Kitchen@S2", "Den@S2"),
            ],
            volumes: [("Kitchen@S1".into(), 11), ("Kitchen@S2".into(), 22)].into(),
            mutes: [("Den@S1".into(), true), ("Den@S2".into(), false)].into(),
            writer: None,
        }
    }

    #[test]
    fn qualified_scene_plans_both_households_without_collapsing_names() {
        let h = houses();
        let plan = plan_apply(&scene(), &h, &[]).unwrap();
        assert!(plan.notes.is_empty());
        assert_eq!(plan.ops.len(), 6);
        let targets = [("OLD_K", "OLD_D", 11, true), ("NEW_K", "NEW_D", 22, false)];
        for (lead, member, level, muted) in targets {
            assert!(plan.ops.iter().any(|op| {
                matches!(op, SceneOp::Join { player, coordinator, .. }
                    if player.0 == member && coordinator.0 == lead)
            }));
            assert!(plan.ops.iter().any(|op| {
                matches!(op, SceneOp::Volume { player, level: v, .. }
                    if player.0 == lead && *v == level)
            }));
            assert!(plan.ops.iter().any(|op| {
                matches!(op, SceneOp::Mute { player, mute, .. }
                    if player.0 == member && *mute == muted)
            }));
        }
        assert_eq!(zones(&scene(), &h).len(), 4);
    }

    #[test]
    fn ambiguous_names_fail_in_every_scene_section() {
        let h = houses();
        for section in 0..4 {
            let mut s = scene();
            match section {
                0 => s.groups[0].coordinator = "Kitchen".into(),
                1 => s.groups[0].members[0] = "Den".into(),
                2 => {
                    s.volumes.insert("Kitchen".into(), 5);
                }
                _ => {
                    s.mutes.insert("Den".into(), true);
                }
            }
            assert!(matches!(
                plan_apply(&s, &h, &[]),
                Err(SceneError::Core(CoreError::AmbiguousRoom { .. }))
            ));
        }
    }

    #[test]
    fn qualifier_and_player_id_cannot_put_one_room_in_two_groups() {
        let h = houses();
        for reference in ["Den@HH_OLD", "OLD_D", "den@s1"] {
            let mut s = scene();
            s.groups[1].members.push(reference.into());
            assert!(matches!(check(&s, &h), Err(SceneError::RoomTwice(_))));
        }
        let mut s = scene();
        s.groups[0].members = vec!["Den@S2".into()];
        assert!(matches!(
            check(&s, &h),
            Err(SceneError::CrossHousehold { .. })
        ));
    }

    #[test]
    fn captured_references_preserve_maps_and_survive_household_reordering() {
        let mut h = houses();
        let mut levels = BTreeMap::new();
        let mut identities = Vec::new();
        for (i, house) in h.iter().enumerate() {
            for room in &house.rooms {
                let key = captured_name(&h, i, room).unwrap();
                assert!(key.contains("@HH_"));
                assert!(levels.insert(key.clone(), identities.len()).is_none());
                identities.push((key, room.primary.clone()));
            }
        }
        assert_eq!(levels.len(), 4);
        h.reverse();
        for (key, id) in identities {
            assert_eq!(named_room(&h, &key).unwrap().unwrap().1.primary, id);
        }
    }

    #[test]
    fn a_missing_qualified_room_never_falls_back_to_the_other_household() {
        let mut h = houses();
        h.remove(0);
        let mut s = scene();
        s.groups = vec![group("Kitchen@HH_OLD", "Den@HH_OLD")];
        s.volumes = [("Kitchen@HH_OLD".into(), 5)].into();
        s.mutes.clear();
        let plan = plan_apply(&s, &h, &[]).unwrap();
        assert!(plan.ops.is_empty());
        assert_eq!(plan.notes.len(), 2);
        assert!(zones(&s, &h).is_empty());
    }

    #[test]
    fn persistence_uses_exact_names_not_prefixes_or_reserved_sets() {
        let h = houses();
        for missing in ["Kit", "all", "everywhere", "here", "Kit@S1"] {
            assert!(named_room(&h, missing).unwrap().is_none());
            assert!(matches!(
                canonical_room_name(&h, missing),
                Err(CoreError::UnknownRoom { .. })
            ));
        }
        // Name resolution does not require network addresses.
        let mut h = h;
        h[0].players.clear();
        let room = named_room(&h, "Kitchen@HH_OLD").unwrap().unwrap().1;
        assert_eq!(room.primary.0, "OLD_K");
    }

    #[test]
    fn canonical_names_keep_explicit_household_scope_and_fold_apostrophes() {
        let h = vec![household("HH_ONE", Generation::S2, &[("ONE", "Ada’s Studio")])];
        assert_eq!(
            canonical_room_name(&h, " ada's studio ").unwrap(),
            "Ada’s Studio"
        );
        assert_eq!(
            canonical_room_name(&h, "ada's studio@s2").unwrap(),
            "Ada’s Studio@HH_ONE"
        );
        assert_eq!(canonical_room_name(&h, "ONE").unwrap(), "Ada’s Studio@HH_ONE");
        assert_eq!(captured_name(&h, 0, &h[0].rooms[0]).unwrap(), "Ada’s Studio");
    }

    #[test]
    fn same_household_duplicates_fall_back_to_player_ids() {
        let h = vec![household(
            "HH_ONE",
            Generation::S2,
            &[("ONE", "Office"), ("TWO", "Office")],
        )];
        for room in &h[0].rooms {
            assert_eq!(captured_name(&h, 0, room).unwrap(), room.primary.0);
        }
        assert!(matches!(
            canonical_room_name(&h, "Office@S2"),
            Err(CoreError::AmbiguousRoom { .. })
        ));
    }

    #[test]
    fn unlabeled_households_never_persist_discovery_order_indices() {
        let mut h = houses();
        for house in &mut h {
            house.id = None;
            house.players.clear();
        }
        let key = captured_name(&h, 0, &h[0].rooms[0]).unwrap();
        assert_eq!(key, "OLD_K");
        h.reverse();
        assert_eq!(named_room(&h, &key).unwrap().unwrap().1.primary.0, "OLD_K");
    }

    #[test]
    fn literal_at_names_cannot_hijack_generated_qualifiers() {
        let mut h = houses();
        h[1].rooms[1].name = "Kitchen@HH_OLD".into();
        let key = captured_name(&h, 0, &h[0].rooms[0]).unwrap();
        assert_ne!(key, "Kitchen@HH_OLD");
        assert_eq!(named_room(&h, &key).unwrap().unwrap().1.primary.0, "OLD_K");
    }

    #[test]
    fn case_only_name_differences_do_not_collide_in_saved_scene_validation() {
        let mut h = houses();
        h[1].rooms[0].name = "kitchen".into();
        let old = captured_name(&h, 0, &h[0].rooms[0]).unwrap();
        let new = captured_name(&h, 1, &h[1].rooms[0]).unwrap();
        assert_ne!(normalize_room(&old), normalize_room(&new));
        assert_eq!(named_room(&h, &old).unwrap().unwrap().1.primary.0, "OLD_K");
        assert_eq!(named_room(&h, &new).unwrap().unwrap().1.primary.0, "NEW_K");
    }
}
