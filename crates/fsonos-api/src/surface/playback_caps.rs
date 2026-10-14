//! Quiet hours and room caps for music a capped caller starts, not just the
//! volumes it sets: before a play, a resume, a DJ start, a join, a move or a
//! party, each room the music will sound in that stands above the caller's
//! cap there now is lowered to it ([`Guard::playback_caps`]), with a
//! `VOLUME_CLAMPED` note. An agent cannot start the music at 80 at 2 a.m. in
//! a room someone left loud.

use fsonos_core::{HouseholdState, control};
use fsonos_types::PlayerId;

use super::Surface;
use crate::failure::{Failure, NoteCode};
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
            // A room that does not answer is left out: the command fails
            // there too, or the room plays nothing.
            match control::volume(&*self.transport, households, &room.primary) {
                Ok(level) if level > cap => over.push((room, level, cap, why)),
                Ok(_) => {}
                Err(e) => tracing::warn!(room = room.name.as_str(), "volume unread: {e}"),
            }
        }
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
