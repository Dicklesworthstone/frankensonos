//! Authored concrete selections share the guarded ordered queue writer.

use super::{SceneHooks, sources};
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

/// The same bound as immediate ordered-track playback and catalog expansion.
pub const MAX_SCENE_TRACKS: usize = control::MAX_SPOTIFY_TRACK_BATCH;

/// A canonical identity and optional cosmetic metadata, never account details
/// or a captured renderer URI. Duplicate entries are distinct occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneTrack {
    pub source_uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// Validate persisted selections too: authoring normalization must not be the
/// only protection before a legacy or externally edited scene reaches a player.
pub fn validate_track_selection(tracks: &[SceneTrack]) -> Result<(), String> {
    if tracks.is_empty() || tracks.len() > MAX_SCENE_TRACKS {
        return Err(format!(
            "a scene track selection needs between 1 and {MAX_SCENE_TRACKS} tracks or episodes"
        ));
    }
    for track in tracks {
        if !track
            .source_uri
            .strip_prefix("spotify:track:")
            .or_else(|| track.source_uri.strip_prefix("spotify:episode:"))
            .is_some_and(|id| id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
        {
            return Err(
                "each scene selection must be a canonical Spotify track or episode with a 22-character id"
                    .into(),
            );
        }
        if track.title.as_ref().is_some_and(|title| {
            title.trim().is_empty()
                || title.chars().count() > 256
                || title
                    .chars()
                    .any(|c| c.is_control() && !matches!(c, '\t' | '\n' | '\r'))
        }) {
            return Err("a scene track title must contain 1 to 256 display characters".into());
        }
    }
    Ok(())
}

pub(super) fn recall<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[SceneTrack],
    start: bool,
    hooks: &dyn SceneHooks,
) -> Result<(), String> {
    validate_track_selection(tracks)?;
    if let Some(result) = hooks.load_track_selection(households, coordinator, tracks, start) {
        return result;
    }
    sources::confirm_audience(transport, households, coordinator).map_err(|e| e.to_string())?;
    let host = control::locate(households, coordinator)
        .map_err(|e| e.to_string())?
        .ip;
    let destination = control::TrackBatchDestination::read(transport, host, coordinator)
        .map_err(|e| e.to_string())?;
    // The initial audience read and the destination snapshot must refer to
    // the same expected group, not an independently regrouped replacement.
    sources::confirm_audience(transport, households, coordinator).map_err(|e| e.to_string())?;
    let selected: Vec<_> = tracks
        .iter()
        .map(|track| {
            (
                track.source_uri.as_str(),
                track.title.as_deref().unwrap_or(&track.source_uri),
            )
        })
        .collect();
    let result = if hooks.queue_replacement_started(coordinator) {
        let result = if start {
            control::play_spotify_tracks_bound_with_receipt(
                transport,
                households,
                coordinator,
                &selected,
                &destination,
            )
        } else {
            control::load_spotify_tracks_bound_with_receipt(
                transport,
                households,
                coordinator,
                &selected,
                &destination,
            )
        };
        result.map(|receipt| hooks.queue_replacement_finished(coordinator, receipt))
    } else if start {
        control::play_spotify_tracks_bound(
            transport,
            households,
            coordinator,
            &selected,
            &destination,
        )
    } else {
        control::load_spotify_tracks_bound(
            transport,
            households,
            coordinator,
            &selected,
            &destination,
        )
    };
    result.map_err(|error: CoreError| error.to_string())
}

#[cfg(test)]
mod tests;
