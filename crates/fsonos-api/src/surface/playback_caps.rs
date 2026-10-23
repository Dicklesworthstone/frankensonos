//! Quiet hours and room caps for music a capped caller starts, not just the
//! volumes it sets: before a play, a resume, a DJ start or skip, an unmute,
//! a join, a move, a party or a scene, each room the music will sound in that
//! stands above the caller's cap there now is lowered to it
//! ([`Guard::playback_caps`]), with a `VOLUME_CLAMPED` note. An agent cannot
//! start the music at 80 at 2 a.m. in a room someone left loud, and a room
//! whose level cannot be read is not started at all.

use fsonos_core::policy::Client;
use fsonos_core::scenes::{SceneOp, ScenePlan};
use fsonos_core::{HouseholdState, Room, control};
use fsonos_types::PlayerId;
use std::collections::HashMap;

use super::Surface;
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::Command;

impl Surface {
    /// Lower the rooms `command` will start music in to `guard`'s caps, if
    /// any stands above them; the notes saying so.
    pub(super) fn lower_before_playing(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<Vec<Note>, Failure> {
        // Without a DJ here a DJ start is refused: nothing to lower for it.
        if matches!(command, Command::Dj { .. } | Command::DjSteer { .. }) && self.dj.is_none() {
            return Ok(Vec::new());
        }
        let mut over = Vec::new();
        for (room, cap, why) in guard.playback_caps(households, command) {
            if let Some(level) = self.above(households, room, cap)? {
                over.push((room, level, cap, why));
            }
        }
        self.lower(households, over)
    }

    /// Before a scene's sources start: lower each room `plan` starts music
    /// in (grouped as the plan leaves the house) that stands above the
    /// lowest cap any of `clients` is held to there now. A room the plan
    /// sets a level for is left to it: those levels are already within the
    /// caps (`bound_volumes`).
    pub(super) fn lower_scene_rooms(
        &self,
        households: &[HouseholdState],
        plan: &ScenePlan,
        clients: &[&Client],
    ) -> Result<Vec<Note>, Failure> {
        let guards: Vec<Guard<'_>> = clients.iter().map(|client| self.guard(client)).collect();
        let mut over = Vec::new();
        for room in scene_sounding_rooms(households, plan) {
            let cap = guards
                .iter()
                .filter_map(|g| g.policy.cap_for(&room.name, g.client, g.clock.now()))
                .min_by_key(|(cap, _)| *cap);
            if let Some((cap, why)) = cap
                && let Some(level) = self.above(households, room, cap)?
            {
                over.push((room, level, cap, why));
            }
        }
        self.lower(households, over)
    }

    /// `room`'s level when it stands above `cap`. A room whose level cannot
    /// be read is refused, before anything is sent: its group would still
    /// play there, at whatever level it was left (p7vq.13).
    fn above(
        &self,
        households: &[HouseholdState],
        room: &Room,
        cap: u8,
    ) -> Result<Option<u8>, Failure> {
        let level = control::volume(&*self.transport, households, &room.primary).map_err(|e| {
            Failure::new(
                ErrorCode::PlayerUnreachable,
                format!(
                    "could not read {}'s volume to hold it to its cap of {cap} ({e}); \
                     nothing was started",
                    room.name
                ),
            )
        })?;
        Ok((level > cap).then_some(level))
    }

    /// Set each room in `over` (room, level, cap, why) to its cap; the
    /// notes saying so.
    fn lower(
        &self,
        households: &[HouseholdState],
        over: Vec<(&Room, u8, u8, String)>,
    ) -> Result<Vec<Note>, Failure> {
        if over.is_empty() {
            return Ok(Vec::new());
        }
        // A fade in flight there would raise them again a step later.
        let players: Vec<PlayerId> = over.iter().map(|(r, ..)| r.primary.clone()).collect();
        self.volumes_changing(households, &players);
        let mut notes = Vec::new();
        for (room, level, cap, why) in over {
            control::set_volume(&*self.transport, households, &room.primary, cap)?;
            notes.push(Note {
                code: NoteCode::VolumeClamped,
                detail: format!("{} lowered from {level} to {cap} first: {why}", room.name),
            });
        }
        Ok(notes)
    }
}

/// The rooms `plan` starts music in, grouped as the plan leaves the house:
/// every room of a group it plays a source in (not one it only loads,
/// paused), resumes or starts the DJ in, and every room it joins to a group
/// or unmutes (as a join and an unmute do for [`Guard::playback_caps`]); not
/// a room the plan sets a level for.
fn scene_sounding_rooms<'h>(households: &'h [HouseholdState], plan: &ScenePlan) -> Vec<&'h Room> {
    let rooms = || households.iter().flat_map(|h| h.rooms.iter());
    let mut lead: HashMap<&PlayerId, &PlayerId> =
        rooms().map(|r| (&r.primary, &r.coordinator)).collect();
    let (mut joining, mut starting, mut leveled) = (Vec::new(), Vec::new(), Vec::new());
    for op in &plan.ops {
        match op {
            SceneOp::Leave { player, .. } => {
                lead.insert(player, player);
            }
            SceneOp::Join {
                player,
                coordinator,
                ..
            } => {
                lead.insert(player, coordinator);
                joining.push(player);
            }
            SceneOp::Favorite { coordinator, .. }
            | SceneOp::Uri { coordinator, .. }
            | SceneOp::Queue { coordinator, .. }
            | SceneOp::QueueBackup { coordinator, .. }
            | SceneOp::Dj { coordinator, .. }
            | SceneOp::Play { coordinator } => {
                if op.starts() {
                    starting.push(coordinator);
                }
            }
            SceneOp::Volume { player, .. } => leveled.push(player),
            // An unmuted room sounds at its own level, as a joining one does.
            SceneOp::Mute {
                player,
                mute: false,
                ..
            } => joining.push(player),
            SceneOp::Mute { mute: true, .. } | SceneOp::Pause { .. } => {}
        }
    }
    rooms()
        .filter(|r| !leveled.contains(&&r.primary))
        .filter(|r| {
            joining.contains(&&r.primary)
                || lead.get(&r.primary).is_some_and(|c| starting.contains(c))
        })
        .collect()
}
