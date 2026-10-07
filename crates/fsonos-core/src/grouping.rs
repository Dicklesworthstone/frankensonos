//! Grouping rooms: the room-level verbs on top of [`control::join`] and
//! [`control::leave`].
//!
//! People group rooms, not players. A stereo pair or a home-theater set moves
//! as one: the command goes to the room's primary and its bonded players
//! follow (the speakers enforce this; a group verb sent to a pair's hidden
//! half fails). A group never spans households, so rooms of the other
//! household are reported rather than sent. Both verbs work from one
//! topology snapshot and carry on past a room that fails, reporting each
//! room's outcome; refresh the topology afterwards.

use crate::rooms::ControlTarget;
use crate::{HouseholdState, control};
use fsonos_proto::Transport;

/// Why a room was left as it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// It is the room the others join.
    Target,
    /// It already plays in the target's group.
    AlreadyGrouped,
    /// It belongs to the other household; groups never span households.
    OtherHousehold,
    /// It already plays on its own.
    AlreadyAlone,
    /// It was listed more than once.
    Duplicate,
}

/// What a grouping verb did, room by room.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupingOutcome {
    /// Rooms the speakers accepted the change for.
    pub moved: Vec<String>,
    /// Rooms left as they were, and why.
    pub skipped: Vec<(String, Skipped)>,
    /// Rooms whose command failed, with the error.
    pub failed: Vec<(String, String)>,
}

impl GroupingOutcome {
    /// Whether every room either moved or needed nothing.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        self.failed.is_empty()
    }
}

/// Bring every room in `rooms` into the group `target` plays in.
pub fn group<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    target: &ControlTarget<'_>,
    rooms: &[ControlTarget<'_>],
) -> GroupingOutcome {
    let mut out = GroupingOutcome::default();
    let mut seen = Vec::new();
    for r in rooms {
        let name = r.room.name.clone();
        let skip = if seen.contains(&&r.room.primary) {
            Some(Skipped::Duplicate)
        } else if !std::ptr::eq(r.household, target.household) {
            Some(Skipped::OtherHousehold)
        } else if r.room.primary == target.room.primary {
            Some(Skipped::Target)
        } else if r.room.coordinator == target.room.coordinator {
            Some(Skipped::AlreadyGrouped)
        } else {
            None
        };
        seen.push(&r.room.primary);
        match skip {
            Some(why) => out.skipped.push((name, why)),
            None => match control::join(t, households, &r.room.primary, &target.room.coordinator) {
                Ok(()) => out.moved.push(name),
                Err(e) => out.failed.push((name, e.to_string())),
            },
        }
    }
    out
}

/// Take every room in `rooms` out of its group into a group of its own. A
/// room that leads others hands the group to one of them (the speakers pick),
/// and that group keeps playing.
pub fn ungroup<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    rooms: &[ControlTarget<'_>],
) -> GroupingOutcome {
    let mut out = GroupingOutcome::default();
    let mut seen = Vec::new();
    for r in rooms {
        let name = r.room.name.clone();
        if seen.contains(&&r.room.primary) {
            out.skipped.push((name, Skipped::Duplicate));
            continue;
        }
        seen.push(&r.room.primary);
        if plays_alone(r) {
            out.skipped.push((name, Skipped::AlreadyAlone));
            continue;
        }
        match control::leave(t, households, &r.room.primary) {
            Ok(()) => out.moved.push(name),
            Err(e) => out.failed.push((name, e.to_string())),
        }
    }
    out
}

/// Whether `r`'s group holds nothing but `r`'s own players.
fn plays_alone(r: &ControlTarget<'_>) -> bool {
    r.household
        .groups
        .iter()
        .find(|g| g.coordinator == r.room.coordinator)
        .is_none_or(|g| g.members.iter().all(|m| r.room.players.contains(m)))
}
