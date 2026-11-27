//! Save reviewed native occurrences as an exact reusable selection.

use super::{SceneDefinition, SceneNativeSelectionDto, SceneSourceDto, scene_room};
use crate::surface::sonos_playlists::{SonosPlaylistTrackMode, SonosPlaylistTrackRequest};
use crate::{Failure, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::scenes::{NativeTrackSelection, SceneSource};
use std::collections::BTreeMap;

pub(super) fn requested(
    source: &SceneSourceDto,
) -> Result<Option<(&str, &SceneNativeSelectionDto)>, Failure> {
    if source.kind != "sonos_playlist_tracks" {
        if source.selection.is_some() {
            return Err(Failure::invalid(
                "only a sonos_playlist_tracks source takes a native selection",
            ));
        }
        return Ok(None);
    }
    if source.uri.is_some()
        || source.mood.is_some()
        || source.direction.is_some()
        || source.queue.is_some()
        || source.playlist.is_some()
        || source.tracks.is_some()
    {
        return Err(Failure::invalid(
            "a native track scene accepts only its SQ: id in name and a reviewed selection; renderer metadata and saved bindings cannot be authored",
        ));
    }
    let id = source.name.as_deref().map(str::trim).unwrap_or_default();
    let selection = source.selection.as_ref().ok_or_else(|| {
        Failure::invalid(
            "a native track scene needs selection with its source zone, positions and expected_revision",
        )
    })?;
    if selection.positions.is_empty()
        || selection.positions.len() > fsonos_core::control::MAX_TRACK_BATCH
    {
        return Err(Failure::invalid(
            "select between 1 and 10 saved playlist occurrences",
        ));
    }
    for &position in &selection.positions {
        SonosPlaylistTrackRequest {
            zone: selection.zone.clone(),
            playlist: id.into(),
            position,
            expected_revision: selection.expected_revision.clone(),
            mode: SonosPlaylistTrackMode::Play,
        }
        .validate()?;
    }
    Ok(Some((id, selection)))
}

pub(super) fn resolve(
    surface: &Surface,
    definition: &SceneDefinition,
    households: &[HouseholdState],
) -> Result<BTreeMap<usize, SceneSource>, Failure> {
    let mut selected = BTreeMap::new();
    for (index, group) in definition.groups.iter().enumerate() {
        let Some(source) = &group.source else {
            continue;
        };
        let Some((id, request)) = requested(source)? else {
            continue;
        };
        let (_, origin) = scene_room(households, &request.zone)?;
        let selection = NativeTrackSelection::capture(
            &*surface.transport,
            households,
            &origin.coordinator,
            id,
            &request.positions,
            &request.expected_revision,
        )
        .map_err(Failure::from)?;
        selected.insert(index, SceneSource::SonosPlaylistTracks { selection });
    }
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn source() -> serde_json::Value {
        json!({
            "kind": "sonos_playlist_tracks", "name": "SQ:7",
            "selection": {
                "zone": "Kitchen@S2", "positions": [3, 1, 3],
                "expected_revision": "observed-native-playlist"
            }
        })
    }

    #[test]
    fn exact_native_selection_requires_observed_occurrences_and_no_authored_metadata() {
        let dto: SceneSourceDto = serde_json::from_value(source()).unwrap();
        let (id, selection) = requested(&dto).unwrap().unwrap();
        assert_eq!(id, "SQ:7");
        assert_eq!(selection.positions, [3, 1, 3]);
        for field in ["uri", "mood", "queue", "playlist", "tracks"] {
            let mut value = source();
            value[field] = match field {
                "queue" => json!({
                    "coordinator": "RINCON_1", "update_id": 1, "track": 1,
                    "position_secs": 0
                }),
                "playlist" => json!({"household": "foreign", "id": "SQ:7", "title": "Other"}),
                "tracks" => json!([{"source_uri": "x-file-cifs://owner.invalid/music.flac"}]),
                _ => json!("caller-supplied"),
            };
            let dto: SceneSourceDto = serde_json::from_value(value).unwrap();
            assert!(requested(&dto).is_err(), "{field}");
        }
        for positions in [vec![], vec![0], vec![1; 11]] {
            let mut dto: SceneSourceDto = serde_json::from_value(source()).unwrap();
            dto.selection.as_mut().unwrap().positions = positions;
            assert!(requested(&dto).is_err());
        }
        let mut dto: SceneSourceDto = serde_json::from_value(source()).unwrap();
        dto.selection.as_mut().unwrap().expected_revision.clear();
        assert!(requested(&dto).is_err());
        for kind in ["keep", "tracks", "sonos_playlist", "dj"] {
            dto.kind = kind.into();
            assert!(requested(&dto).is_err());
        }
    }
}
