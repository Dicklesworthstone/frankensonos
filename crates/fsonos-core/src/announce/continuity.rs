//! Let unannounced rooms keep their live source when their coordinator
//! briefly leaves for a clip, then return that transport natively.

mod proof;

use super::recovery::Effects;
use crate::{CoreError, HouseholdState, Room, control};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::PlayerId;
use proof::{Binding, Layout, Playback, changed};

#[derive(Clone)]
pub(super) struct Continuation {
    pub(super) coordinator: PlayerId,
    survivor: PlayerId,
    targets: Vec<PlayerId>,
    binding: Binding,
    separated: Layout,
    ready: bool,
}

/// A room's own source immediately after it left the temporary group.
/// Retaining this evidence prevents a subsequent user action in that room
/// from being overwritten by its rejoin.
pub(super) struct Released {
    player: PlayerId,
    playback: Playback,
}

pub(super) struct Returned {
    playback: Playback,
}

impl Returned {
    pub(super) fn check<T: Transport + ?Sized>(
        &self,
        t: &T,
        entry: &Continuation,
    ) -> Result<(), CoreError> {
        entry.binding.check(
            t,
            &entry.binding.whole(&entry.coordinator),
            &entry.coordinator,
        )?;
        self.playback.confirm(
            t,
            entry.binding.host(&entry.coordinator)?,
            &entry.coordinator,
        )
    }
}

pub(super) fn release<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    player: &PlayerId,
    stopped_clip: Option<&str>,
) -> Result<Released, CoreError> {
    let host = control::locate(std::slice::from_ref(household), player)?.ip;
    let playback = Playback::capture(t, host, player)?;
    if stopped_clip.is_some_and(|clip| !playback.is_stopped_clip(clip)) {
        return Err(changed(
            "the clip was changed or restarted before returning its room",
        ));
    }
    Ok(Released {
        player: player.clone(),
        playback,
    })
}

pub(super) fn contains(entries: &[Continuation], player: &PlayerId) -> bool {
    entries.iter().any(|entry| entry.targets.contains(player))
}

/// Detach the targeted members first, so the targeted coordinator can only
/// hand music to an untargeted room. This also handles several partially
/// targeted groups in one announcement, rather than just its lead room.
pub(super) fn prepare<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    targets: &[&Room],
    effects: &mut Effects,
) -> Result<(), CoreError> {
    let targeted = |room: &Room| targets.iter().any(|r| r.primary == room.primary);
    for room in targets.iter().filter(|r| r.is_group_coordinator()) {
        let group: Vec<_> = household
            .rooms
            .iter()
            .filter(|r| r.coordinator == room.coordinator)
            .collect();
        let Some(survivor) = group.iter().find(|r| !targeted(r)) else {
            continue;
        };
        if room.primary != room.coordinator {
            return Err(changed(
                "a degraded coordinator room cannot be handed off safely",
            ));
        }
        let (binding, separated) = Binding::capture(t, household, &room.coordinator)?;
        let mut departing: Vec<_> = group
            .iter()
            .filter(|r| targeted(r) && r.primary != room.primary)
            .map(|r| r.primary.clone())
            .collect();
        departing.push(room.primary.clone());
        let entry = Continuation {
            coordinator: room.primary.clone(),
            survivor: survivor.primary.clone(),
            targets: departing.clone(),
            binding,
            separated,
            ready: false,
        };
        // Record before the first mutation: even an uncertain departure must
        // never fall back to replaying this coordinator's now-displaced queue.
        effects.continuations.push(entry);
        let entry = effects.continuations.last_mut().expect("just inserted");
        for player in departing {
            effects.groups.push(player.clone());
            effects.sources.push(player.clone());
            entry.depart(t, &player)?;
        }
        entry.ready = true;
    }
    Ok(())
}

impl Continuation {
    fn depart<T: Transport + ?Sized>(&mut self, t: &T, player: &PlayerId) -> Result<(), CoreError> {
        let now = self.binding.check(t, &self.separated, &self.coordinator)?;
        let host = self.binding.host(&self.coordinator)?;
        let before = Playback::capture(t, host, &self.coordinator)?;
        let is_coordinator = player == &self.coordinator;
        let heir = if is_coordinator {
            // Match control::leave's first remaining room, now necessarily
            // untargeted. The topology can order rooms differently from a UI.
            let group = now
                .groups
                .iter()
                .find(|g| g.coordinator == self.coordinator)
                .ok_or_else(|| changed("the source group disappeared"))?;
            self.survivor = group
                .members
                .iter()
                .find_map(|p| {
                    now.rooms
                        .iter()
                        .find(|r| r.primary == *p && !r.players.contains(player))
                        .map(|r| r.primary.clone())
                })
                .ok_or_else(|| changed("no untargeted room can keep the music"))?;
            Some(&self.survivor)
        } else {
            None
        };
        let mut after = self.separated.clone();
        after.depart(self.binding.room(player)?, heir);
        self.binding.check(t, &self.separated, &self.coordinator)?;
        before.confirm(t, host, &self.coordinator)?;
        control::leave(t, std::slice::from_ref(&now), player)?;
        let playing = if is_coordinator {
            &self.survivor
        } else {
            &self.coordinator
        };
        self.binding.after(t, &after, playing)?;
        before.transferred(t, self.binding.host(playing)?, playing)?;
        self.binding.after(t, &after, playing)?;
        self.separated = after;
        Ok(())
    }

    /// Every native mutation runs once. After a lost reply only fresh reads
    /// can prove it; failure never authorizes a snapshot replay or queue clear.
    pub(super) fn restore<T: Transport + ?Sized>(
        &self,
        t: &T,
        released: &[Released],
    ) -> Result<Returned, CoreError> {
        if !self.ready {
            return Err(changed("the initial live handoff was not fully confirmed"));
        }
        let mut layout = self.separated.clone();
        self.binding.check(t, &layout, &self.survivor)?;
        // Read the survivor now, not the pre-announcement snapshot. Its
        // current occurrence, position, queue edits and paused state win.
        let live = Playback::capture(t, self.binding.host(&self.survivor)?, &self.survivor)?;
        let mut returning = vec![self.coordinator.clone()];
        returning.extend(
            self.targets
                .iter()
                .filter(|p| **p != self.coordinator)
                .cloned(),
        );
        for player in returning {
            let own = released
                .iter()
                .find(|r| r.player == player)
                .ok_or_else(|| changed("a returning room has no post-clip ownership proof"))?;
            let now = self.binding.check(t, &layout, &self.survivor)?;
            own.playback
                .confirm(t, self.binding.host(&player)?, &player)?;
            live.confirm(t, self.binding.host(&self.survivor)?, &self.survivor)?;
            own.playback
                .confirm(t, self.binding.host(&player)?, &player)?;
            control::join(t, std::slice::from_ref(&now), &player, &self.survivor)?;
            layout.join(self.binding.room(&player)?, &self.survivor);
            self.binding.after(t, &layout, &self.survivor)?;
            live.confirm(t, self.binding.host(&self.survivor)?, &self.survivor)?;
        }
        // Keeping the old survivor in the group is essential: RejoinGroup=0
        // would detach an unannounced room while returning coordination.
        self.binding.check(t, &layout, &self.survivor)?;
        live.confirm(t, self.binding.host(&self.survivor)?, &self.survivor)?;
        let sent = match soap::delegate_group_and_rejoin(
            t,
            self.binding.host(&self.survivor)?,
            &self.coordinator,
        ) {
            Err(error)
                if !matches!(
                    &error,
                    ProtoError::Network { .. } | ProtoError::Malformed(_)
                ) =>
            {
                return Err(error.into());
            }
            result => result,
        };
        layout.delegate(&self.survivor, &self.coordinator);
        let proof = (|| {
            self.binding.after(t, &layout, &self.coordinator)?;
            live.transferred(t, self.binding.host(&self.coordinator)?, &self.coordinator)?;
            self.binding.after(t, &layout, &self.coordinator)?;
            Ok(())
        })();
        match proof {
            Ok(()) => Ok(Returned { playback: live }),
            Err(error) => Err(sent.err().map_or(error, CoreError::from)),
        }
    }
}
