//! Restore attempted effects only while the announcement still owns the state.

use super::{GroupPlan, HouseholdAnnouncement, refreshed};
use crate::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::PlayerId;
use std::time::Duration;

const CLIP_SETTLE: Duration = Duration::from_millis(750);

/// Recorded before dispatch, since a failed reply does not prove no effect.
#[derive(Clone, Default)]
pub(super) struct Effects {
    pub groups: Vec<PlayerId>,
    pub sources: Vec<PlayerId>,
    pub volumes: Vec<(PlayerId, u8)>,
    pub mutes: Vec<PlayerId>,
    pub before_clip: Option<String>,
    pub clip: Option<String>,
    pub preserve: Option<String>,
}

/// A changed temporary group belongs to whoever changed it, not to cleanup.
pub(super) fn check_group(
    now: &HouseholdState,
    before: &HouseholdState,
    plan: &GroupPlan,
) -> Result<(), String> {
    let mut expected = vec![plan.lead.clone()];
    expected.extend(plan.joins.iter().cloned());
    if !plan.lead_leaves {
        expected.extend(
            before
                .rooms
                .iter()
                .filter(|r| r.coordinator == plan.lead)
                .map(|r| r.primary.clone()),
        );
    }
    let actual: Vec<_> = now
        .rooms
        .iter()
        .filter(|r| r.coordinator == plan.lead)
        .map(|r| &r.primary)
        .collect();
    if expected.iter().all(|p| actual.contains(&p)) && actual.iter().all(|p| expected.contains(p)) {
        Ok(())
    } else {
        Err("announcement grouping changed; newer playback and grouping were left alone".into())
    }
}

fn preserve(snaps: &[ZoneSnapshot], why: &str, report: &mut HouseholdAnnouncement) {
    for snap in snaps {
        report.restored.push((
            snap.coordinator.clone(),
            RestoreReport {
                restored: Vec::new(),
                skipped: Aspect::ALL
                    .into_iter()
                    .map(|a| (a, why.to_string()))
                    .collect(),
            },
        ));
    }
}

pub(super) fn put_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    snaps: &[ZoneSnapshot],
    effects: &Effects,
    report: &mut HouseholdAnnouncement,
) {
    if let Some(why) = &effects.preserve {
        preserve(snaps, why, report);
        return;
    }
    let mut effects = effects.clone();
    if let Some(clip) = &effects.clip {
        let owned = (|| -> Result<bool, String> {
            let now = refreshed(t, household).map_err(|e| e.to_string())?;
            check_group(&now, household, plan)?;
            let host = control::locate(std::slice::from_ref(&now), &plan.lead)
                .map_err(|e| e.to_string())?
                .ip;
            let media = soap::get_media_info(t, host).map_err(|e| e.to_string())?;
            if media.uri == *clip {
                return Ok(true);
            }
            if report.outcome.is_err() && effects.before_clip.as_ref() == Some(&media.uri) {
                // A rejected source install left the music running. Restore
                // its levels/grouping without seeking or restarting that source.
                return Ok(false);
            }
            Err(
                "announcement source changed before restoration; newer playback was left alone"
                    .into(),
            )
        })();
        match owned {
            Ok(true) => {}
            Ok(false) => effects.sources.retain(|p| p != &plan.lead),
            Err(why) => {
                preserve(snaps, &why, report);
                return;
            }
        }
    }
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
        if snap
            .levels
            .iter()
            .any(|l| effects.volumes.iter().any(|(p, _)| p == &l.player))
        {
            aspects.push(Aspect::Volume);
        }
        if snap
            .levels
            .iter()
            .any(|l| effects.mutes.contains(&l.player))
        {
            aspects.push(Aspect::Mute);
        }
        if aspects.is_empty() {
            continue;
        }
        let mut changed = snap.clone();
        // The announcement never changed ordering. Keep the current setting,
        // including a newer choice made by the owner while the clip played.
        changed.play_mode = None;
        changed.levels.retain(|l| {
            effects.volumes.iter().any(|(p, _)| p == &l.player) || effects.mutes.contains(&l.player)
        });
        let restore = || {
            let now = refreshed(t, household)?;
            snapshot::restore_vetted(t, std::slice::from_ref(&now), &changed, &aspects, &|op| {
                vet(t, &now, &changed, &effects, op)
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

fn vet<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    snap: &ZoneSnapshot,
    effects: &Effects,
    op: &RestoreOp,
) -> Result<RestoreOp, String> {
    let host = |p: &PlayerId| {
        control::locate(std::slice::from_ref(household), p)
            .map(|p| p.ip)
            .map_err(|e| e.to_string())
    };
    match op {
        RestoreOp::SetVolume { player, level } => {
            let expected = effects
                .volumes
                .iter()
                .find(|(p, _)| p == player)
                .map(|(_, v)| *v);
            let current = soap::get_volume(t, host(player)?).map_err(|e| e.to_string())?;
            if current != *level && Some(current) != expected {
                return Err("volume changed during announcement; keeping the newer level".into());
            }
        }
        RestoreOp::SetMute { player, mute } => {
            let current = soap::get_mute(t, host(player)?).map_err(|e| e.to_string())?;
            if current != *mute && (current || !effects.mutes.contains(player)) {
                return Err("mute changed during announcement; keeping the newer setting".into());
            }
        }
        RestoreOp::SeekTrack(_) | RestoreOp::SeekPosition(_) | RestoreOp::Play => {
            let expected = match &snap.source {
                SnapshotSource::Queue { .. } => format!("x-rincon-queue:{}#0", snap.coordinator.0),
                SnapshotSource::Uri { uri, .. } => uri.clone(),
                SnapshotSource::Nothing => String::new(),
            };
            let current = soap::get_media_info(t, host(&snap.coordinator)?)
                .map_err(|e| e.to_string())?
                .uri;
            if current != expected {
                return Err(
                    "source was not restored; refusing to seek or play another source".into(),
                );
            }
        }
        _ => {}
    }
    Ok(op.clone())
}
