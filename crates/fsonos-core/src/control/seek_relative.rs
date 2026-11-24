//! A relative seek uses a continuous clock sample and never repeats its offset.
//! Recheck the original occurrence after preparation so a track transition or
//! listener seek cannot inherit an offset computed for the previous position.
//! Lost replies use the same source, boot and queue proof as an absolute seek.

use super::seek_absolute::Observation;
use crate::CoreError;
use fsonos_proto::control::{self as soap, PositionInfo};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

const MAX_CONFIRM_AGE: Duration = Duration::from_secs(30);

pub(super) fn seek<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    by: i64,
) -> Result<u32, CoreError> {
    let observed = Instant::now();
    let before = soap::get_position_info(t, host)?;
    destination(&before, by)?;
    let proof = Observation::for_relative(t, host, coordinator, before.clone(), observed);
    super::require_coordinator(t, host, coordinator)?;
    if let Some(proof) = &proof {
        proof.confirm_destination(t, host, coordinator)?;
    }
    let current = soap::get_position_info(t, host)?;
    if !continuous(&before, &current, observed.elapsed()) {
        return Err(ProtoError::Malformed(
            "the track or playback position changed while preparing the relative seek; \
             no seek was sent; read playback before trying again"
                .into(),
        )
        .into());
    }
    let target = destination(&current, by)?;
    let started = Instant::now();
    match soap::seek_position(t, host, target) {
        Ok(()) => Ok(target),
        Err(error) => {
            let ambiguous = matches!(
                &error,
                ProtoError::Network { .. } | ProtoError::Malformed(_)
            );
            if ambiguous
                && proof.is_some_and(|proof| proof.recovered(t, host, coordinator, target, started))
            {
                tracing::warn!(
                    cause = %error,
                    "relative seek observed without repeating its offset"
                );
                Ok(target)
            } else {
                // The read-back error must not hide the original write failure.
                Err(error.into())
            }
        }
    }
}

fn destination(before: &PositionInfo, by: i64) -> Result<u32, ProtoError> {
    let position = before.position_secs.ok_or_else(|| {
        ProtoError::Malformed(
            "relative seek needs a reported playback position; \
             read state again or use an absolute seek"
                .into(),
        )
    })?;
    if before.uri.trim().is_empty() || before.uri.trim() == "NOT_IMPLEMENTED" {
        return Err(ProtoError::Malformed(
            "relative seek needs a current track, not an empty source".into(),
        ));
    }
    let duration = before.duration_secs.filter(|d| *d > 0);
    if duration.is_some_and(|d| position > d) {
        return Err(ProtoError::Malformed(
            "relative seek received a position beyond the track duration".into(),
        ));
    }
    let requested = i64::from(position).saturating_add(by).max(0);
    // A known end is the next track; stay on its last whole second instead.
    let last = duration.map_or(i64::from(u32::MAX), |d| i64::from(d - 1));
    Ok(u32::try_from(requested.min(last)).unwrap_or(u32::MAX))
}

fn same_track(before: &PositionInfo, after: &PositionInfo) -> bool {
    // URI alone cannot distinguish two adjacent copies of the same track.
    before.uri == after.uri
        && before.track == after.track
        && before.duration_secs == after.duration_secs
}

/// Natural progress can advance only by the elapsed observation time, allowing
/// one second for whole-second renderer rounding. A manual backward/forward
/// seek, another duplicate occurrence, or an old sample requires a fresh request.
fn continuous(before: &PositionInfo, after: &PositionInfo, age: Duration) -> bool {
    if age > MAX_CONFIRM_AGE || !same_track(before, after) {
        return false;
    }
    let (Some(before), Some(after)) = (before.position_secs, after.position_secs) else {
        return false;
    };
    after >= before && u64::from(after - before) <= age.as_secs() + 1
}

pub(super) fn reached(
    before: &PositionInfo,
    after: &PositionInfo,
    target: u32,
    state: TransportState,
    age: Duration,
) -> bool {
    if age > MAX_CONFIRM_AGE || !same_track(before, after) {
        return false;
    }
    let Some(position) = after.position_secs else {
        return false;
    };
    if after.duration_secs.is_some_and(|d| d > 0 && position >= d) {
        return false;
    }
    match state {
        TransportState::Paused | TransportState::Stopped => position == target,
        TransportState::Playing => {
            // Include whole-second renderer rounding, but never accept a
            // backwards jump or an unbounded stale observation as confirmation.
            let elapsed = u32::try_from(age.as_secs()).unwrap_or(u32::MAX);
            position >= target && position <= target.saturating_add(elapsed).saturating_add(1)
        }
        TransportState::Transitioning | TransportState::Unknown => false,
    }
}

#[cfg(test)]
mod tests;
