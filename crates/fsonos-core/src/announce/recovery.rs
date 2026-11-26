//! Restore attempted effects only while the announcement still owns the state.

mod untouched;
pub(super) use untouched::Origin;

use super::{GroupPlan, HouseholdAnnouncement, continuity, refreshed, topology};
use crate::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use crate::{CoreError, HouseholdState, Room, control, heal};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::time::{Duration, Instant};

const CLIP_SETTLE: Duration = Duration::from_millis(750);

/// The longest wait between tries at players that stopped answering.
const MAX_PAUSE: Duration = Duration::from_secs(10);

/// Successful group writes can settle after their acknowledgement. Only
/// topology reads repeat before installing the clip, never the joins.
pub(super) fn wait_for_group<T: Transport + ?Sized>(
    t: &T,
    before: &HouseholdState,
    plan: &GroupPlan,
) -> Result<(), CoreError> {
    let lead = control::locate(std::slice::from_ref(before), &plan.lead)?;
    let mut tried = std::collections::HashSet::new();
    let witnesses: Vec<_> = std::iter::once(lead)
        .chain(&before.players)
        .filter(|player| tried.insert(player.ip))
        .collect();
    let started = Instant::now();
    let wait = Duration::from_secs(3);
    let mut read_any = false;
    let mut last = ProtoError::Malformed("the announcement group was not confirmed".into()).into();
    loop {
        for player in &witnesses {
            if read_any && started.elapsed() >= wait {
                return Err(last);
            }
            read_any = true;
            match topology::read(t, before, player) {
                Ok(now) => match check_group(&now, before, plan) {
                    Ok(()) => return Ok(()),
                    Err(why) => last = ProtoError::Malformed(why).into(),
                },
                Err(error) => last = error,
            }
        }
        let remaining = wait.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            return Err(last);
        }
        std::thread::sleep(Duration::from_millis(100).min(remaining));
    }
}

/// Recorded before dispatch, since a failed reply does not prove no effect.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
    pub continuations: Vec<continuity::Continuation>,
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
        .collect();
    if expected.iter().all(|player| {
        before
            .rooms
            .iter()
            .find(|room| room.primary == *player)
            .is_some_and(|room| {
                actual
                    .iter()
                    .any(|current| topology::same_room(room, current))
            })
    }) && actual.iter().all(|room| expected.contains(&room.primary))
    {
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

#[allow(clippy::too_many_arguments)]
pub(super) fn put_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    snaps: &[ZoneSnapshot],
    effects: &Effects,
    origins: &[Origin],
    capture_started: Instant,
    give_up: Instant,
    restore_cap: Option<&(dyn Fn(&Room) -> u8 + Sync)>,
    report: &mut HouseholdAnnouncement,
) {
    if let Some(why) = &effects.preserve {
        preserve(snaps, why, report);
        return;
    }
    let mut effects = effects.clone();
    let mut clip_is_source = true;
    if let Some(clip) = &effects.clip {
        let owned = patiently(give_up, &|| Ok(None), || {
            source(t, household, plan, &effects, clip)
        });
        match owned {
            Ok(Ok(Source::Clip)) => {}
            Ok(Ok(Source::Untouched)) => {
                effects.sources.retain(|p| p != &plan.lead);
                clip_is_source = false;
            }
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
    let (released, cleanup_plan) = match release_rooms(
        t,
        household,
        plan,
        &effects,
        clip_is_source,
        give_up,
        report,
    ) {
        Ok(rooms) => rooms,
        Err(why) => {
            preserve(snaps, &why, report);
            return;
        }
    };
    let still_owned = || -> Result<Option<String>, CoreError> {
        let Some(clip) = &effects.clip else {
            return Ok(None);
        };
        Ok(match source(t, household, &cleanup_plan, &effects, clip)? {
            Source::Clip | Source::Untouched => None,
            Source::Newer(why) => Some(why),
        })
    };
    for snap in snaps {
        put_zone_back(
            t,
            household,
            snap,
            &effects,
            &released,
            origins,
            capture_started,
            give_up,
            &still_owned,
            restore_cap,
            report,
        );
    }
}

/// Shrink the expected temporary group after each confirmed leave. A
/// subsequent retry must not mistake our own cleanup for an external regroup.
fn release_rooms<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    effects: &Effects,
    clip_is_source: bool,
    give_up: Instant,
    report: &mut HouseholdAnnouncement,
) -> Result<(Vec<continuity::Released>, GroupPlan), String> {
    let mut released = Vec::new();
    if continuity::contains(&effects.continuations, &plan.lead) {
        let stopped_clip = effects.clip.as_deref().filter(|_| clip_is_source);
        match continuity::release(t, household, &plan.lead, stopped_clip) {
            Ok(owned) => released.push(owned),
            Err(error) => {
                return Err(error.to_string());
            }
        }
    }
    let mut cleanup_plan = plan.clone();
    for room in plan.joins.iter().filter(|r| effects.groups.contains(r)) {
        let still_owned = || -> Result<Option<String>, CoreError> {
            let Some(clip) = &effects.clip else {
                return Ok(None);
            };
            Ok(match source(t, household, &cleanup_plan, effects, clip)? {
                Source::Clip | Source::Untouched => None,
                Source::Newer(why) => Some(why),
            })
        };
        let left = patiently(give_up, &still_owned, || {
            if let Some(why) = still_owned()? {
                return Err(ProtoError::Malformed(why).into());
            }
            let now = refreshed(t, household)?;
            // An unapplied join must not detach the room from its old group.
            if now.coordinator_of(room) == Some(&plan.lead) {
                control::leave(t, std::slice::from_ref(&now), room)?;
            }
            if continuity::contains(&effects.continuations, room) {
                return continuity::release(t, household, room, None).map(Some);
            }
            Ok(None)
        });
        let error = match left {
            Ok(Ok(owned)) => {
                released.extend(owned);
                cleanup_plan.joins.retain(|p| p != room);
                None
            }
            Ok(Err(why)) => Some(why),
            Err(error) => Some(error.to_string()),
        };
        if let Some(why) = error {
            report.restore_failed.push((room.clone(), why.clone()));
            if !effects.continuations.is_empty() {
                return Err(why);
            }
        }
    }
    Ok((released, cleanup_plan))
}

/// Restore what the announcement changed in `snap`'s zone.
#[allow(clippy::too_many_arguments)]
fn put_zone_back<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    snap: &ZoneSnapshot,
    effects: &Effects,
    released: &[continuity::Released],
    origins: &[Origin],
    capture_started: Instant,
    give_up: Instant,
    still_owned: Owned<'_>,
    restore_cap: Option<&(dyn Fn(&Room) -> u8 + Sync)>,
    report: &mut HouseholdAnnouncement,
) {
    let continuation = effects
        .continuations
        .iter()
        .find(|entry| entry.coordinator == snap.coordinator);
    let continued = [
        Aspect::Group,
        Aspect::Source,
        Aspect::Position,
        Aspect::Transport,
    ];
    let returned = match continuation
        .map(|entry| entry.restore(t, released))
        .transpose()
    {
        Ok(returned) => returned,
        Err(error) => {
            let why = error.to_string();
            report
                .restore_failed
                .push((snap.coordinator.clone(), why.clone()));
            report
                .restored
                .push((snap.coordinator.clone(), left_alone(&Aspect::ALL, &why)));
            return;
        }
    };
    let aspects = changed_aspects(snap, effects, continuation.is_some());
    if aspects.is_empty() && continuation.is_none() {
        // Cap callbacks may have interrupted this zone's sleep fade even
        // though cancellation or admission refusal prevented its first write.
        // Only a complete fresh proof can authorize that sleep continuation.
        let verified = untouched::confirm(t, household, snap, origins, capture_started);
        report.restored.push((
            snap.coordinator.clone(),
            match verified {
                Ok(()) => RestoreReport::default(),
                Err(why) => left_alone(&Aspect::ALL, &why),
            },
        ));
        return;
    }
    let preparation_only = effects.clip.is_none()
        && continuation.is_none()
        && !effects.sources.contains(&snap.coordinator)
        && !snap
            .members
            .iter()
            .any(|player| effects.groups.contains(player));
    if preparation_only && let Err(why) = untouched::source_current(t, household, snap, origins) {
        report
            .restored
            .push((snap.coordinator.clone(), left_alone(&Aspect::ALL, &why)));
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
            // Every dispatch needs the original authority, including a retry
            // after an unreachable player or the 402 clip-settling backoff.
            if preparation_only {
                untouched::source_current(t, household, snap, origins)?;
            }
            if let Some((entry, proof)) = continuation.zip(returned.as_ref()) {
                proof.check(t, entry).map_err(|error| error.to_string())?;
            }
            vet(t, household, &now, &changed, effects, op, restore_cap)
        })
    };
    let owned = || match continuation.zip(returned.as_ref()) {
        Some((entry, proof)) => proof.check(t, entry).map(|()| None),
        None => still_owned(),
    };
    // Preserve the bounded retry for a player still tearing down its clip.
    let result = patiently(give_up, &owned, || match restore() {
        Err(CoreError::Proto(ProtoError::SoapFault { code: 402, .. })) => {
            std::thread::sleep(CLIP_SETTLE);
            restore()
        }
        result => result,
    });
    match result {
        Ok(Ok(mut r)) => {
            if continuation.is_some() {
                r.restored.splice(0..0, continued);
            }
            // A preparation cancelled before source installation may have
            // changed only a room's level. Restoring that level alone cannot
            // authorize its old sleep on a source or group changed meanwhile.
            if effects.clip.is_none()
                && r.skipped.is_empty()
                && let Err(why) = untouched::confirm(t, household, snap, origins, capture_started)
            {
                r.skipped
                    .extend(Aspect::ALL.into_iter().map(|aspect| (aspect, why.clone())));
            }
            report.restored.push((snap.coordinator.clone(), r));
        }
        Ok(Err(why)) => report
            .restored
            .push((snap.coordinator.clone(), left_alone(&aspects, &why))),
        Err(e) => report
            .restore_failed
            .push((snap.coordinator.clone(), e.to_string())),
    }
}

fn changed_aspects(snap: &ZoneSnapshot, effects: &Effects, continuing: bool) -> Vec<Aspect> {
    let mut aspects = Vec::new();
    if !continuing && snap.members.iter().any(|p| effects.groups.contains(p)) {
        aspects.push(Aspect::Group);
    }
    if !continuing && effects.sources.contains(&snap.coordinator) {
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
    aspects
}

fn vet<T: Transport + ?Sized>(
    t: &T,
    before: &HouseholdState,
    dispatch: &HouseholdState,
    snap: &ZoneSnapshot,
    effects: &Effects,
    op: &RestoreOp,
    restore_cap: Option<&(dyn Fn(&Room) -> u8 + Sync)>,
) -> Result<RestoreOp, String> {
    let household = refreshed(t, before).map_err(|error| error.to_string())?;
    if !snap.members.iter().all(|primary| {
        before
            .rooms
            .iter()
            .find(|room| room.primary == *primary)
            .is_some_and(|room| {
                household
                    .rooms
                    .iter()
                    .any(|current| topology::same_room(room, current))
            })
    }) {
        return Err(
            "announcement room membership changed during restoration; newer intent was left alone"
                .into(),
        );
    }
    let host = |id: &PlayerId| bound_host(dispatch, &household, id);
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
    require_dispatch(t, dispatch, &household, &snap.coordinator, op)?;
    if let (Some(cap), RestoreOp::SetVolume { player, level }) = (restore_cap, op) {
        let room = household
            .rooms
            .iter()
            .find(|room| room.primary == *player)
            .ok_or_else(|| "announcement room disappeared before volume restoration".to_string())?;
        // Ownership above compares the actual level with the original write,
        // not a newly lowered cap. A listener's newer level still wins. Check
        // policy after the endpoint reads, as quiet hours can change mid-clip.
        return Ok(RestoreOp::SetVolume {
            player: player.clone(),
            level: (*level).min(cap(room)),
        });
    }
    Ok(op.clone())
}

fn bound_host(
    dispatch: &HouseholdState,
    current: &HouseholdState,
    player: &PlayerId,
) -> Result<IpAddr, String> {
    let bound = control::locate(std::slice::from_ref(dispatch), player)
        .map_err(|error| error.to_string())?
        .ip;
    let observed = control::locate(std::slice::from_ref(current), player)
        .map_err(|error| error.to_string())?
        .ip;
    if bound == observed {
        Ok(bound)
    } else {
        Err("announcement player address changed during restoration; the old endpoint was left alone".into())
    }
}

/// The snapshot executor uses its retained dispatch addresses. A fresh read
/// at a moved player's new address cannot authorize a write at the old one.
/// Recheck actual endpoint ownership after the aspect reads too.
fn require_dispatch<T: Transport + ?Sized>(
    t: &T,
    dispatch: &HouseholdState,
    current: &HouseholdState,
    lead: &PlayerId,
    op: &RestoreOp,
) -> Result<(), String> {
    let host = |id: &PlayerId| bound_host(dispatch, current, id);
    let player = |id: &PlayerId| {
        control::require_player(t, host(id)?, id).map_err(|error| error.to_string())
    };
    let coordinator =
        || control::require_coordinator(t, host(lead)?, lead).map_err(|error| error.to_string());
    match op {
        RestoreOp::Join { player: id } => {
            coordinator()?;
            player(id)?;
        }
        RestoreOp::Leave { player: id }
        | RestoreOp::SetVolume { player: id, .. }
        | RestoreOp::SetMute { player: id, .. }
        | RestoreOp::RestoreQueue { player: id, .. } => player(id)?,
        _ => coordinator()?,
    }
    Ok(())
}
