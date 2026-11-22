//! Authored selections use the same pure input codec as immediate track playback.

use super::SceneSourceDto;
use crate::Failure;
use crate::surface::queue::PlayTracksRequest;
use fsonos_core::scenes::{self, SceneSource, SceneTrack};

pub(super) fn source(dto: &SceneSourceDto) -> Result<SceneSource, Failure> {
    if dto.name.is_some()
        || dto.uri.is_some()
        || dto.mood.is_some()
        || dto.direction.is_some()
        || dto.queue.is_some()
        || dto.playlist.is_some()
    {
        return Err(Failure::invalid(
            "a tracks scene source accepts only its ordered `tracks` selection",
        ));
    }
    let tracks = dto.tracks.as_ref().ok_or_else(|| {
        Failure::invalid("a tracks scene source needs its ordered `tracks` selection")
    })?;
    // Scene room resolution is separate. This codec performs no discovery:
    // it canonicalizes the exact same URIs and titles as play_tracks.
    let request = PlayTracksRequest {
        zone: "scene".into(),
        tracks: tracks.clone(),
        expected_revision: None,
    }
    .normalized()?;
    let tracks: Vec<_> = request
        .tracks
        .into_iter()
        .map(|track| SceneTrack {
            source_uri: track.source_uri,
            title: track.title,
        })
        .collect();
    scenes::validate_track_selection(&tracks).map_err(Failure::invalid)?;
    Ok(SceneSource::Tracks { tracks })
}

#[cfg(test)]
mod tests;
