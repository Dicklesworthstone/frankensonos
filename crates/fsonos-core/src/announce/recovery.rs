//! Restore only what this announcement attempted to change.

use super::{GroupPlan, HouseholdAnnouncement, refreshed};
use crate::snapshot::{self, Aspect, ZoneSnapshot};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::time::Duration;

const CLIP_SETTLE: Duration = Duration::from_millis(750);

/// Recorded before dispatch, since a failed reply does not prove no effect.
#[derive(Default)]
pub(super) struct Effects {
    pub groups: Vec<PlayerId>,
    pub sources: Vec<PlayerId>,
    pub volumes: Vec<PlayerId>,
    pub mutes: Vec<PlayerId>,
}

pub(super) fn put_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    snaps: &[ZoneSnapshot],
    effects: &Effects,
    report: &mut HouseholdAnnouncement,
) {
    for room in plan.joins.iter().filter(|r| effects.groups.contains(r)) {
        let left = refreshed(t, household).and_then(|now| {
            // An unapplied join must not detach the room from its old group.
            if now.coordinator_of(room) == Some(&plan.lead) {
                control::leave(t, std::slice::from_ref(&now), room)?;
            }
            Ok(())
        });
        if let Err(e) = left {
            report.restore_failed.push((room.clone(), e.to_string()));
        }
    }
    for snap in snaps {
        let mut aspects = Vec::new();
        if snap.members.iter().any(|p| effects.groups.contains(p)) {
            aspects.push(Aspect::Group);
        }
        if effects.sources.contains(&snap.coordinator) {
            aspects.extend([Aspect::Source, Aspect::Position, Aspect::Transport]);
        }
        if snap.levels.iter().any(|l| effects.volumes.contains(&l.player)) {
            aspects.push(Aspect::Volume);
        }
        if snap.levels.iter().any(|l| effects.mutes.contains(&l.player)) {
            aspects.push(Aspect::Mute);
        }
        if aspects.is_empty() {
            continue;
        }
        let mut changed = snap.clone();
        changed.levels.retain(|l| {
            effects.volumes.contains(&l.player) || effects.mutes.contains(&l.player)
        });
        let restore = || {
            refreshed(t, household).and_then(|now| {
                snapshot::restore_only(t, std::slice::from_ref(&now), &changed, &aspects)
            })
        };
        // Preserve the bounded retry for a player still tearing down its clip.
        let result = match restore() {
            Err(CoreError::Proto(ProtoError::SoapFault { code: 402, .. })) => {
                std::thread::sleep(CLIP_SETTLE);
                restore()
            }
            result => result,
        };
        match result {
            Ok(r) => report.restored.push((snap.coordinator.clone(), r)),
            Err(e) => report
                .restore_failed
                .push((snap.coordinator.clone(), e.to_string())),
        }
    }
}
