//! Restore only the playback a temporary stream still owns. This is bounded
//! read-before-write protection, not an atomic transaction with other clients.

use fsonos_core::snapshot::{self, Aspect, RestoreOp, RestoreReport, SnapshotSource, ZoneSnapshot};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{Transport, control as soap};
use fsonos_types::TransportState;

const PLAYBACK: [Aspect; 3] = [Aspect::Source, Aspect::Position, Aspect::Transport];

fn preserve(why: &str) -> RestoreReport {
    RestoreReport {
        restored: Vec::new(),
        skipped: PLAYBACK
            .into_iter()
            .map(|aspect| (aspect, why.to_owned()))
            .collect(),
    }
}

fn same_source(expected: &str, actual: &str) -> bool {
    expected == actual
        || expected
            .strip_prefix("x-rincon-mp3radio://")
            .zip(actual.strip_prefix("http://"))
            .is_some_and(|(expected, actual)| expected == actual)
        || actual
            .strip_prefix("x-rincon-mp3radio://")
            .zip(expected.strip_prefix("http://"))
            .is_some_and(|(actual, expected)| expected == actual)
}

fn check_group(now: &HouseholdState, before: &ZoneSnapshot) -> Result<(), String> {
    if now.id != before.household
        || now.coordinator_of(&before.coordinator) != Some(&before.coordinator)
    {
        return Err("stream household or coordinator changed; newer grouping was left alone".into());
    }
    let actual: Vec<_> = now
        .rooms
        .iter()
        .filter(|room| room.coordinator == before.coordinator)
        .map(|room| &room.primary)
        .collect();
    if actual.len() != before.members.len()
        || !before.members.iter().all(|member| actual.contains(&member))
    {
        return Err("stream group membership changed; newer grouping was left alone".into());
    }
    Ok(())
}

fn refreshed<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    before: &ZoneSnapshot,
) -> Result<HouseholdState, String> {
    let host = household
        .player(&before.coordinator)
        .ok_or_else(|| "stream coordinator disappeared".to_owned())?
        .ip;
    let topology = get_zone_group_state(transport, host).map_err(|error| error.to_string())?;
    let mut now = household.clone();
    now.apply_topology(&topology);
    check_group(&now, before)?;
    Ok(now)
}

fn original_uri(before: &ZoneSnapshot) -> String {
    match &before.source {
        SnapshotSource::Nothing => String::new(),
        SnapshotSource::Queue { .. } => format!("x-rincon-queue:{}#0", before.coordinator.0),
        SnapshotSource::Uri { uri, .. } => uri.clone(),
    }
}

fn check_queue<T: Transport + ?Sized>(
    transport: &T,
    host: std::net::IpAddr,
    before: &ZoneSnapshot,
) -> Result<(), String> {
    if let SnapshotSource::Queue { update_id, .. } = before.source {
        let queue = content::browse(
            transport,
            host,
            content::QUEUE,
            BrowseFlag::DirectChildren,
            0,
            1,
        )
        .map_err(|error| error.to_string())?;
        if queue.update_id != update_id {
            return Err("queue changed; refusing to replace, seek or resume it".into());
        }
    }
    Ok(())
}

/// Encoder startup can take time. Do not displace a new source/group that
/// appeared after the initial snapshot while capture was warming up.
pub(super) fn verify_start<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    before: &ZoneSnapshot,
) -> Result<(), String> {
    let household = households
        .iter()
        .find(|household| household.player(&before.coordinator).is_some())
        .ok_or_else(|| "stream coordinator disappeared before startup".to_owned())?;
    let now = refreshed(transport, household, before)?;
    let host = now
        .player(&before.coordinator)
        .ok_or_else(|| "stream coordinator disappeared before startup".to_owned())?
        .ip;
    let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
    if !same_source(&original_uri(before), &media.uri) {
        return Err("playback changed while preparing capture; stream was not started".into());
    }
    let state = soap::get_transport_info(transport, host).map_err(|error| error.to_string())?;
    if state.state != before.transport_state {
        return Err("transport changed while preparing capture; stream was not started".into());
    }
    check_queue(transport, host, before)
}

/// `started` means Play was acknowledged. A failed dispatch may nevertheless
/// have installed the stream URI, so callers must also invoke this after an
/// attempted but failed Play. Never restore grouping or member levels: stream
/// capture does not own them. Preserve a later pause/stop by not auto-resuming.
pub(super) fn restore<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    before: &ZoneSnapshot,
    temporary: &str,
    started: bool,
) -> Result<RestoreReport, CoreError> {
    let Some(household) = households
        .iter()
        .find(|household| household.player(&before.coordinator).is_some())
    else {
        return Ok(preserve("stream coordinator disappeared; nothing was restored"));
    };
    let now = match refreshed(transport, household, before) {
        Ok(now) => now,
        Err(why) => return Ok(preserve(&why)),
    };
    let host = now
        .player(&before.coordinator)
        .ok_or_else(|| CoreError::UnknownPlayer(before.coordinator.0.clone()))?
        .ip;
    let current = soap::get_media_info(transport, host)?;
    if !same_source(temporary, &current.uri) {
        return Ok(preserve("stream source changed; newer playback was left alone"));
    }
    let state = soap::get_transport_info(transport, host)?.state;
    let keep_silent = started && state != TransportState::Playing;
    let mut previous = before.clone();
    if keep_silent {
        previous.transport_state = state;
    }
    let original = original_uri(&previous);
    let mut report = snapshot::restore_vetted(
        transport,
        std::slice::from_ref(&now),
        &previous,
        &PLAYBACK,
        &|operation| {
            // Re-read at each write, not just once before the entire restore.
            let current = refreshed(transport, household, &previous)?;
            let host = current
                .player(&previous.coordinator)
                .ok_or_else(|| "stream coordinator disappeared".to_owned())?
                .ip;
            let expected = match operation {
                RestoreOp::SetUri { .. } | RestoreOp::PlayQueue | RestoreOp::Stop => temporary,
                RestoreOp::SeekTrack(_) | RestoreOp::SeekPosition(_) | RestoreOp::Play => &original,
                _ => return Err("stream cleanup cannot change grouping, volume or mute".into()),
            };
            let media = soap::get_media_info(transport, host).map_err(|error| error.to_string())?;
            if !same_source(expected, &media.uri) {
                return Err(
                    "source changed during stream cleanup; no dependent write was sent".into(),
                );
            }
            check_queue(transport, host, &previous)?;
            Ok(operation.clone())
        },
    )?;
    if keep_silent && before.transport_state == TransportState::Playing {
        report.skipped.push((
            Aspect::Transport,
            "stream was no longer playing; the previous source was not auto-resumed".into(),
        ));
    }
    Ok(report)
}

#[cfg(test)]
mod tests;
