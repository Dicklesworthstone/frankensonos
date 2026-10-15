//! Restore attempted effects only while the announcement still owns the state.

use super::{GroupPlan, HouseholdAnnouncement, refreshed};
use crate::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use crate::{CoreError, HouseholdState, control, heal};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::PlayerId;
use std::time::{Duration, Instant};

const CLIP_SETTLE: Duration = Duration::from_millis(750);

/// The longest wait between tries at players that stopped answering.
const MAX_PAUSE: Duration = Duration::from_secs(10);

/// Recorded before dispatch, since a failed reply does not prove no effect.
#[derive(Clone, Default)]
pub(super) struct Effects {
    pub groups: Vec<PlayerId>,
    pub sources: Vec<PlayerId>,
    pub volumes: Vec<(PlayerId, u8)>,
    pub mutes: Vec<PlayerId>,
    pub before_clip: Option<String>,
    pub clip: Option<String>,
    /// The lead accepted the clip as its source (`play_uri` answered).
    pub installed: bool,
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

/// What the lead plays as the house is put back.
enum Source {
    /// The clip: the announcement still owns it.
    Clip,
    /// The music from before, untouched by a rejected source install.
    Untouched,
    /// Something newer, left alone for the reason given.
    Newer(String),
}

fn source<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    effects: &Effects,
    clip: &str,
) -> Result<Source, CoreError> {
    let now = refreshed(t, household)?;
    if let Err(why) = check_group(&now, household, plan) {
        return Ok(Source::Newer(why));
    }
    let host = control::locate(std::slice::from_ref(&now), &plan.lead)?.ip;
    let media = soap::get_media_info(t, host)?;
    if media.uri == clip {
        return Ok(Source::Clip);
    }
    if !effects.installed && effects.before_clip.as_ref() == Some(&media.uri) {
        // A rejected source install left the music running. Restore
        // its levels/grouping without seeking or restarting that source.
        return Ok(Source::Untouched);
    }
    Ok(Source::Newer(
        "announcement source changed before restoration; newer playback was left alone".into(),
    ))
}

/// Whether the announcement still owns what a retried step would write
/// over: `Some(why)` when it doesn't.
type Owned<'a> = &'a dyn Fn() -> Result<Option<String>, CoreError>;

/// `op`, tried again while the players don't answer at all (never after a
/// fault or a refusal), until `give_up`: a Wi-Fi drop on a real house can
/// outlast the clip, and the house should still be put back after it. The
/// owner may have started something while they were silent, so `owned` is
/// asked before each retry; `Ok(Err(why))` means it was left alone.
fn patiently<R>(
    give_up: Instant,
    owned: Owned<'_>,
    mut op: impl FnMut() -> Result<R, CoreError>,
) -> Result<Result<R, String>, CoreError> {
    let mut pause = Duration::from_millis(500);
    let mut retry = false;
    loop {
        let attempt = match if retry { owned() } else { Ok(None) } {
            Ok(None) => op().map(Ok),
            Ok(Some(why)) => Ok(Err(why)),
            Err(e) => Err(e),
        };
        match attempt {
            Err(e) if heal::is_unreachable(&e) && Instant::now() + pause <= give_up => {
                tracing::info!(error = %e, "putting back after an announcement: waiting for the players");
                std::thread::sleep(pause);
                pause = (pause * 2).min(MAX_PAUSE);
                retry = true;
            }
            result => return result,
        }
    }
}

/// Every aspect in `aspects` left alone for `why`.
fn left_alone(aspects: &[Aspect], why: &str) -> RestoreReport {
    RestoreReport {
        restored: Vec::new(),
        skipped: aspects.iter().map(|a| (*a, why.to_owned())).collect(),
    }
}

pub(super) fn put_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    snaps: &[ZoneSnapshot],
    effects: &Effects,
    give_up: Instant,
    report: &mut HouseholdAnnouncement,
) {
    if let Some(why) = &effects.preserve {
        preserve(snaps, why, report);
        return;
    }
    let mut effects = effects.clone();
    if let Some(clip) = &effects.clip {
        let owned = patiently(give_up, &|| Ok(None), || {
            source(t, household, plan, &effects, clip)
        });
        match owned {
            Ok(Ok(Source::Clip)) => {}
            Ok(Ok(Source::Untouched)) => effects.sources.retain(|p| p != &plan.lead),
            Ok(Ok(Source::Newer(why)) | Err(why)) => {
                preserve(snaps, &why, report);
                return;
            }
            // Without the source's identity nothing is put back.
            Err(e) => {
                preserve(snaps, &e.to_string(), report);
                return;
            }
        }
    }
    let still_owned = || -> Result<Option<String>, CoreError> {
        let Some(clip) = &effects.clip else {
            return Ok(None);
        };
        Ok(match source(t, household, plan, &effects, clip)? {
            Source::Clip | Source::Untouched => None,
            Source::Newer(why) => Some(why),
        })
    };
    for room in plan.joins.iter().filter(|r| effects.groups.contains(r)) {
        let left = patiently(give_up, &still_owned, || {
            let now = refreshed(t, household)?;
            // An unapplied join must not detach the room from its old group.
            if now.coordinator_of(room) == Some(&plan.lead) {
                control::leave(t, std::slice::from_ref(&now), room)?;
            }
            Ok(())
        });
        match left {
            Ok(Ok(())) => {}
            Ok(Err(why)) => report.restore_failed.push((room.clone(), why)),
            Err(e) => report.restore_failed.push((room.clone(), e.to_string())),
        }
    }
    for snap in snaps {
        put_zone_back(t, household, snap, &effects, give_up, &still_owned, report);
    }
}

/// Restore what the announcement changed in `snap`'s zone.
fn put_zone_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    snap: &ZoneSnapshot,
    effects: &Effects,
    give_up: Instant,
    still_owned: Owned<'_>,
    report: &mut HouseholdAnnouncement,
) {
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
        return;
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
            vet(t, &now, &changed, effects, op)
        })
    };
    // Preserve the bounded retry for a player still tearing down its clip.
    let result = patiently(give_up, still_owned, || match restore() {
        Err(CoreError::Proto(ProtoError::SoapFault { code: 402, .. })) => {
            std::thread::sleep(CLIP_SETTLE);
            restore()
        }
        result => result,
    });
    match result {
        Ok(Ok(r)) => report.restored.push((snap.coordinator.clone(), r)),
        Ok(Err(why)) => report
            .restored
            .push((snap.coordinator.clone(), left_alone(&aspects, &why))),
        Err(e) => report
            .restore_failed
            .push((snap.coordinator.clone(), e.to_string())),
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
