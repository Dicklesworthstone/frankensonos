//! Resolve authored native playlist IDs before persisting a scene.

use super::{SceneDefinition, SceneSourceDto, scene_room};
use crate::{Failure, Surface};
use fsonos_core::HouseholdState;
use fsonos_core::scenes::SonosPlaylistReference;
use std::collections::BTreeMap;

pub(super) fn requested(source: &SceneSourceDto) -> Result<Option<&str>, Failure> {
    if source.playlist.is_some() {
        return Err(Failure::invalid(
            "playlist identity is learned from the selected household; author a sonos_playlist source with its native SQ: id in name",
        ));
    }
    if source.kind != "sonos_playlist" {
        return Ok(None);
    }
    if source.uri.is_some()
        || source.mood.is_some()
        || source.direction.is_some()
        || source.queue.is_some()
    {
        return Err(Failure::invalid(
            "a sonos_playlist source accepts only its native SQ: id in name",
        ));
    }
    let id = source.name.as_deref().map(str::trim).unwrap_or_default();
    if id.len() > 1024 || !fsonos_core::playlists::native_id(id) {
        return Err(Failure::invalid(
            "a sonos_playlist scene source needs an exact native SQ: id in name",
        )
        .with_hint(
            "Read list_sonos_playlists for the group's room and choose that household's native id.",
        ));
    }
    Ok(Some(id))
}

pub(super) fn resolve(
    surface: &Surface,
    definition: &SceneDefinition,
    households: &[HouseholdState],
) -> Result<BTreeMap<usize, SonosPlaylistReference>, Failure> {
    let mut selected = BTreeMap::new();
    for (index, group) in definition.groups.iter().enumerate() {
        let Some(source) = &group.source else {
            continue;
        };
        let Some(id) = requested(source)? else {
            continue;
        };
        let (_, lead) = scene_room(households, &group.coordinator)?;
        let playlist =
            SonosPlaylistReference::capture(&*surface.transport, households, &lead.primary, id)
                .map_err(Failure::from)?;
        selected.insert(index, playlist);
    }
    Ok(selected)
}
