//! Exact room membership for the groups a scene declares. Other groups stay
//! alone; a level-only scene does not regroup anything.

use super::{HouseholdState, Room, Scene, SceneOp, ScenePlan, find_room, leads};

struct Group<'a> {
    household: &'a HouseholdState,
    lead: &'a Room,
    members: Vec<&'a Room>,
}

pub(super) fn plan(scene: &Scene, households: &[HouseholdState], plan: &mut ScenePlan) {
    // A missing coordinator makes its whole group unavailable. Its members
    // must not suppress the removal of an extra room from an available group.
    let groups: Vec<_> = scene
        .groups
        .iter()
        .filter_map(|g| {
            let (_, household, lead) = find_room(households, &g.coordinator)?;
            Some(Group {
                household,
                lead,
                members: g
                    .members
                    .iter()
                    .filter_map(|name| find_room(households, name).map(|(_, _, r)| r))
                    .collect(),
            })
        })
        .collect();

    // Establish every new coordinator before anything joins it. This also
    // removes a room that becomes a separate scene group's coordinator.
    for g in &groups {
        if !leads(g.lead) {
            plan.ops.push(leave(g.lead));
        }
    }

    for g in &groups {
        if !leads(g.lead) {
            continue;
        }
        for room in &g.household.rooms {
            if room.primary == g.lead.primary
                || room.coordinator != g.lead.coordinator
                || g.members.iter().any(|m| m.primary == room.primary)
            {
                continue;
            }
            // A room assigned elsewhere leaves through that group's existing
            // Leave or Join. Do not detach it twice or interrupt it needlessly.
            let assigned = groups.iter().any(|other| {
                other.lead.primary == room.primary
                    || other.members.iter().any(|m| m.primary == room.primary)
            });
            if !assigned {
                plan.ops.push(leave(room));
            }
        }
    }

    for g in &groups {
        for room in &g.members {
            if !leads(g.lead) || !g.lead.players.contains(&room.coordinator) {
                plan.ops.push(SceneOp::Join {
                    room: room.name.clone(),
                    player: room.primary.clone(),
                    coordinator: g.lead.primary.clone(),
                });
            }
        }
    }
}

fn leave(room: &Room) -> SceneOp {
    SceneOp::Leave {
        room: room.name.clone(),
        player: room.primary.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenes::{SceneGroup, SceneSource, plan_apply, zones};
    use fsonos_types::{HouseholdId, PlayerId, ZoneGroup};
    use std::collections::BTreeMap;

    fn id(n: usize) -> PlayerId {
        PlayerId(format!("room-{n}"))
    }

    fn house(layout: &[usize]) -> Vec<HouseholdState> {
        let rooms: Vec<_> = layout
            .iter()
            .enumerate()
            .map(|(i, lead)| Room {
                name: format!("Room {i}"),
                primary: id(i),
                players: vec![id(i)],
                missing: Vec::new(),
                coordinator: id(*lead),
            })
            .collect();
        let groups = layout
            .iter()
            .enumerate()
            .filter(|(i, lead)| *i == **lead)
            .map(|(i, _)| ZoneGroup {
                coordinator: id(i),
                members: rooms
                    .iter()
                    .filter(|r| r.coordinator == id(i))
                    .map(|r| r.primary.clone())
                    .collect(),
            })
            .collect();
        vec![HouseholdState {
            groups,
            rooms,
            ..HouseholdState::default()
        }]
    }

    fn scene(layout: &[Option<usize>]) -> Scene {
        Scene {
            name: "Layout".into(),
            groups: layout
                .iter()
                .enumerate()
                .filter(|(i, lead)| **lead == Some(*i))
                .map(|(i, _)| SceneGroup {
                    coordinator: format!("Room {i}"),
                    members: layout
                        .iter()
                        .enumerate()
                        .filter(|(j, lead)| *j != i && **lead == Some(i))
                        .map(|(j, _)| format!("Room {j}"))
                        .collect(),
                    source: SceneSource::Keep,
                    playing: None,
                })
                .collect(),
            volumes: BTreeMap::new(),
            mutes: BTreeMap::new(),
            writer: None,
        }
    }

    #[test]
    fn a_solo_scene_detaches_unlisted_rooms_without_restarting_its_lead() {
        let h = house(&[0, 0, 0, 3]);
        let s = scene(&[Some(0), None, None, None]);
        let p = plan_apply(&s, &h, &[]).unwrap();
        assert_eq!(p.ops, [leave(&h[0].rooms[1]), leave(&h[0].rooms[2])]);
        assert_eq!(
            zones(&s, &h),
            [id(0)],
            "snapshot includes the removed rooms"
        );
    }

    #[test]
    fn retains_requested_members_and_does_not_touch_an_unrelated_group() {
        let h = house(&[0, 0, 0, 3, 3]);
        let s = scene(&[Some(0), Some(0), None, None, None]);
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [leave(&h[0].rooms[2])]
        );
    }

    #[test]
    fn a_room_reassigned_to_another_scene_group_moves_only_once() {
        let h = house(&[0, 0, 2]);
        let s = scene(&[Some(0), Some(2), Some(2)]);
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [SceneOp::Join {
                room: "Room 1".into(),
                player: id(1),
                coordinator: id(2),
            }]
        );
    }

    #[test]
    fn a_new_lead_leaves_its_old_group_without_detaching_unrelated_members() {
        let h = house(&[0, 0, 0, 0]);
        let s = scene(&[None, Some(1), Some(1), None]);
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [
                leave(&h[0].rooms[1]),
                SceneOp::Join {
                    room: "Room 2".into(),
                    player: id(2),
                    coordinator: id(1),
                },
            ]
        );
    }

    #[test]
    fn missing_leads_do_not_reserve_rooms_in_other_groups() {
        let h = house(&[0, 0]);
        let mut s = scene(&[Some(0), None]);
        s.groups.push(SceneGroup {
            coordinator: "Missing".into(),
            members: vec!["Room 1".into()],
            source: SceneSource::Keep,
            playing: None,
        });
        let p = plan_apply(&s, &h, &[]).unwrap();
        assert_eq!(p.ops, [leave(&h[0].rooms[1])]);
        assert_eq!(p.notes, ["Missing is not in the house now; skipped"]);
    }

    #[test]
    fn level_only_scenes_keep_the_current_group_layout() {
        let h = house(&[0, 0]);
        let mut s = scene(&[None, None]);
        s.volumes.insert("Room 1".into(), 12);
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [SceneOp::Volume {
                room: "Room 1".into(),
                player: id(1),
                level: 12,
            }]
        );
    }

    #[test]
    fn a_bonded_room_is_detached_once_by_its_primary() {
        let mut h = house(&[0, 0]);
        h[0].rooms[0].players.push(id(10));
        h[0].rooms[1].players.push(id(11));
        h[0].groups[0].members.extend([id(10), id(11)]);
        let s = scene(&[Some(0), None]);
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [leave(&h[0].rooms[1])]
        );
    }

    #[test]
    fn qualified_duplicate_names_remain_household_local() {
        let mut h = house(&[0, 0]);
        h[0].id = Some(HouseholdId("FIRST".into()));
        let mut other = house(&[0, 0]).remove(0);
        other.id = Some(HouseholdId("SECOND".into()));
        for r in &mut other.rooms {
            r.primary.0.push_str("-second");
            r.coordinator.0.push_str("-second");
            for p in &mut r.players {
                p.0.push_str("-second");
            }
        }
        h.push(other);
        let mut s = scene(&[Some(0), None]);
        s.groups[0].coordinator = "Room 0@SECOND".into();
        assert_eq!(
            plan_apply(&s, &h, &[]).unwrap().ops,
            [leave(&h[1].rooms[1])]
        );
    }

    // Model room-level moves independently of SOAP. If a coordinator moves,
    // its remaining rooms elect another coordinator, not the moving room.
    fn detach(layout: &mut [usize], room: usize) {
        if layout[room] == room
            && let Some(heir) = (0..layout.len()).find(|i| *i != room && layout[*i] == room)
        {
            for (i, lead) in layout.iter_mut().enumerate() {
                if i != room && *lead == room {
                    *lead = heir;
                }
            }
        }
        layout[room] = room;
    }

    fn execute(layout: &mut [usize], ops: &[SceneOp]) {
        let index = |p: &PlayerId| (0..layout.len()).find(|i| id(*i) == *p).unwrap();
        let indexed: Vec<_> = ops
            .iter()
            .map(|op| match op {
                SceneOp::Leave { player, .. } => (index(player), None),
                SceneOp::Join {
                    player,
                    coordinator,
                    ..
                } => (index(player), Some(index(coordinator))),
                other => panic!("unexpected layout op {other:?}"),
            })
            .collect();
        for (room, to) in indexed {
            detach(layout, room);
            if let Some(to) = to {
                assert_eq!(layout[to], to, "joins only target established coordinators");
                layout[room] = to;
            }
        }
    }

    #[test]
    fn every_four_room_layout_converges_to_every_partial_scene_and_is_idempotent() {
        const N: usize = 4;
        let layouts: Vec<Vec<usize>> = (0..N.pow(4))
            .map(|mut value| {
                (0..N)
                    .map(|_| {
                        let n = value % N;
                        value /= N;
                        n
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| v.iter().all(|lead| v[*lead] == *lead))
            .collect();
        let scenes: Vec<Vec<Option<usize>>> = (0..(N + 1).pow(4))
            .map(|mut value| {
                (0..N)
                    .map(|_| {
                        let n = value % (N + 1);
                        value /= N + 1;
                        (n < N).then_some(n)
                    })
                    .collect::<Vec<_>>()
            })
            .filter(|v| v.iter().flatten().all(|lead| v[*lead] == Some(*lead)))
            .collect();
        for before in &layouts {
            for wanted in &scenes {
                let s = scene(wanted);
                let p = plan_apply(&s, &house(before), &[]).unwrap();
                let mut after = before.clone();
                execute(&mut after, &p.ops);
                for (i, expected) in wanted.iter().enumerate() {
                    if let Some(lead) = expected {
                        assert_eq!(after[i], *lead, "{before:?} -> {wanted:?}: {p:?}");
                        for j in 0..N {
                            assert_eq!(after[j] == *lead, wanted[j] == Some(*lead));
                        }
                    }
                }
                assert!(plan_apply(&s, &house(&after), &[]).unwrap().ops.is_empty());
            }
        }
    }
}
