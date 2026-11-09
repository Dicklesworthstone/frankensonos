//! A read-only proof for a zone whose announcement never changed an aspect.
//!
//! Cap callbacks can already have suspended its sleep fade. An explicit
//! successful row permits that receipt to resume, but only while every
//! observable part of the original zone is still its own.

use super::{HouseholdState, SnapshotSource, Transport, ZoneSnapshot, refreshed, snapshot};
use crate::{CoreError, control};
use fsonos_proto::control::{self as soap, TransportInfo};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::time::{Duration, Instant};

/// Source identity from before cap callbacks suspend any existing sleep fade.
/// Levels are intentionally read later, after those fades have stopped.
pub(in crate::announce) struct Origin {
    coordinator: PlayerId,
    source: SnapshotSource,
    transport: TransportInfo,
    mode: Option<PlayMode>,
    observed: Instant,
}

impl Origin {
    pub(in crate::announce) fn capture<T: Transport + ?Sized>(
        transport: &T,
        household: &HouseholdState,
        coordinator: &PlayerId,
    ) -> Result<Self, CoreError> {
        let observed = Instant::now();
        let source = snapshot::observe_source(transport, household, coordinator)?;
        Ok(Self {
            coordinator: coordinator.clone(),
            source: source.source,
            transport: source.transport,
            mode: source.play_mode,
            observed,
        })
    }

    pub(in crate::announce) fn matches(&self, snapshot: &ZoneSnapshot) -> bool {
        self.coordinator == snapshot.coordinator
            && self.transport.state == snapshot.transport_state
            && self.mode == snapshot.play_mode
            && same_source(
                &self.source,
                &snapshot.source,
                self.transport.state,
                self.observed.elapsed(),
            )
    }
}

/// Before restoring preparation-only levels, keep a newer source or group
/// entirely alone. This does not require the levels to be restored yet.
pub(super) fn source_current<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    before: &ZoneSnapshot,
    origins: &[Origin],
) -> Result<(), String> {
    let origin = original(origins, before)?;
    let fresh = refreshed(transport, household).map_err(|error| error.to_string())?;
    if !same_group(household, &fresh, &before.coordinator) {
        return Err("announcement preparation group changed; newer intent was left alone".into());
    }
    let current = Origin::capture(transport, &fresh, &before.coordinator)
        .map_err(|error| error.to_string())?;
    let latest = refreshed(transport, &fresh).map_err(|error| error.to_string())?;
    if !same_group(household, &latest, &before.coordinator)
        || current.transport != origin.transport
        || current.mode != origin.mode
        || !same_source(
            &origin.source,
            &current.source,
            origin.transport.state,
            origin.observed.elapsed(),
        )
    {
        return Err("announcement preparation source changed; newer intent was left alone".into());
    }
    Ok(())
}

fn original<'a>(origins: &'a [Origin], before: &ZoneSnapshot) -> Result<&'a Origin, String> {
    origins
        .iter()
        .find(|origin| origin.coordinator == before.coordinator)
        .filter(|origin| origin.matches(before))
        .ok_or_else(|| {
            "announcement source changed while its snapshot was captured; sleep was not resumed"
                .to_string()
        })
}

pub(super) fn confirm<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    before: &ZoneSnapshot,
    origins: &[Origin],
    capture_started: Instant,
) -> Result<(), String> {
    let origin = original(origins, before)?;
    let fresh = refreshed(transport, household).map_err(|error| error.to_string())?;
    if !same_group(household, &fresh, &before.coordinator) {
        return Err("untouched announcement zone changed grouping; sleep was not resumed".into());
    }
    let after = snapshot::capture(
        transport,
        std::slice::from_ref(&fresh),
        &before.coordinator,
        before.captured_at,
    )
    .map_err(|error| format!("could not verify untouched announcement zone: {error}"))?;
    let host = control::locate(std::slice::from_ref(&fresh), &before.coordinator)
        .map_err(|error| error.to_string())?
        .ip;
    let state = soap::get_transport_info(transport, host).map_err(|error| error.to_string())?;
    let latest = refreshed(transport, &fresh).map_err(|error| error.to_string())?;
    if !same_group(household, &latest, &before.coordinator)
        || !agrees(before, &after, capture_started.elapsed())
        || !origin.matches(&after)
        || state != origin.transport
    {
        return Err(
            "untouched announcement zone changed; newer playback or levels were left alone and sleep was not resumed"
                .into(),
        );
    }
    Ok(())
}

fn ids(players: &[PlayerId]) -> Vec<&str> {
    let mut ids: Vec<_> = players.iter().map(|player| player.0.as_str()).collect();
    ids.sort_unstable();
    ids
}

fn same_group(before: &HouseholdState, after: &HouseholdState, coordinator: &PlayerId) -> bool {
    let group = |household: &HouseholdState| {
        household
            .groups
            .iter()
            .find(|group| group.coordinator == *coordinator)
            .map(|group| {
                ids(&group.members)
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
    };
    if before.id != after.id || group(before).is_none() || group(before) != group(after) {
        return false;
    }
    before
        .rooms
        .iter()
        .filter(|room| room.coordinator == *coordinator)
        .all(|room| {
            after.rooms.iter().any(|now| {
                now.primary == room.primary
                    && now.coordinator == room.coordinator
                    && ids(&now.players) == ids(&room.players)
                    && ids(&now.missing) == ids(&room.missing)
            })
        })
}

fn agrees(before: &ZoneSnapshot, after: &ZoneSnapshot, elapsed: Duration) -> bool {
    before.household == after.household
        && before.coordinator == after.coordinator
        && ids(&before.members) == ids(&after.members)
        && before.levels.len() == after.levels.len()
        && before
            .levels
            .iter()
            .all(|level| after.levels.contains(level))
        && before.transport_state == after.transport_state
        && matches!(
            before.transport_state,
            TransportState::Playing | TransportState::Paused | TransportState::Stopped
        )
        && before.play_mode == after.play_mode
        && same_source(
            &before.source,
            &after.source,
            before.transport_state,
            elapsed,
        )
}

fn same_source(
    before: &SnapshotSource,
    after: &SnapshotSource,
    state: TransportState,
    elapsed: Duration,
) -> bool {
    match (before, after) {
        (SnapshotSource::Nothing, SnapshotSource::Nothing) => true,
        (
            SnapshotSource::Queue {
                track,
                position_secs,
                update_id,
            },
            SnapshotSource::Queue {
                track: now_track,
                position_secs: now_position,
                update_id: now_update,
            },
        ) => {
            track == now_track
                && update_id == now_update
                && progresses(Some(*position_secs), Some(*now_position), state, elapsed)
        }
        (
            SnapshotSource::Uri {
                uri,
                metadata,
                position_secs,
            },
            SnapshotSource::Uri {
                uri: now_uri,
                metadata: now_metadata,
                position_secs: now_position,
            },
        ) => {
            uri == now_uri
                && metadata == now_metadata
                && progresses(*position_secs, *now_position, state, elapsed)
        }
        _ => false,
    }
}

fn progresses(
    before: Option<u32>,
    after: Option<u32>,
    state: TransportState,
    elapsed: Duration,
) -> bool {
    match (before, after) {
        (Some(before), Some(after)) if state == TransportState::Playing => {
            let advance = u32::try_from(elapsed.as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1);
            after >= before && after <= before.saturating_add(advance)
        }
        _ => before == after,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_ordinary_progress_of_the_same_occurrence_is_untouched() {
        let source = |track, position_secs, update_id| SnapshotSource::Queue {
            track,
            position_secs,
            update_id,
        };
        let before = source(2, 10, 7);
        let playing = TransportState::Playing;
        let elapsed = Duration::from_secs(2);
        assert!(same_source(&before, &source(2, 13, 7), playing, elapsed));
        for changed in [
            source(3, 10, 7),
            source(2, 10, 8),
            source(2, 9, 7),
            source(2, 14, 7),
        ] {
            assert!(!same_source(&before, &changed, playing, elapsed));
        }
        assert!(!same_source(
            &before,
            &source(2, 11, 7),
            TransportState::Paused,
            elapsed
        ));
        assert!(!progresses(None, Some(0), playing, elapsed));
    }
}
