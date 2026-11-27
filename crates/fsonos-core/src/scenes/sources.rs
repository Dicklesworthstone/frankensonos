//! Read the music a scene needs before changing the house. A successful
//! check is an observation, not a lock: execution still resolves the source
//! again after grouping, and never replays an uncertain playback write.

use super::{SceneError, SceneOp, ScenePlan, check_playlist_sources, validate_track_selection};
use crate::favorites::{self, Favorite, FavoriteError, FavoriteKind};
use crate::topology::read_at;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::collections::hash_map::Entry;

/// Check every permitted favorite, native playlist and canonical Spotify
/// source before a surface lowers volumes, supersedes fades or changes groups.
/// The plan must already have caller and scene-writer policy filtering applied.
/// Only local Sonos catalogs are read; arbitrary stream URLs are not fetched.
/// DJ and saved-queue readiness have their own ownership and execution checks.
pub fn check_sources<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
) -> Result<(), SceneError> {
    // Invalid retained selections fail before any catalog lookup, including
    // lookups for earlier groups in the scene.
    for op in &plan.ops {
        if let SceneOp::Tracks { tracks, .. } = op {
            validate_track_selection(tracks)
                .map_err(|why| CoreError::from(ProtoError::Malformed(why)))?;
        }
    }
    for op in &plan.ops {
        if let SceneOp::SonosPlaylistTracks {
            coordinator,
            selection,
            ..
        } = op
        {
            selection.check_household(households, coordinator)?;
        }
    }
    check_playlist_sources(t, households, plan)?;
    let mut catalogs = HashMap::new();
    for op in &plan.ops {
        match op {
            SceneOp::Favorite {
                coordinator, name, ..
            } => {
                let house = households
                    .iter()
                    .position(|house| house.player(coordinator).is_some())
                    .ok_or_else(|| CoreError::UnknownPlayer(coordinator.0.clone()))?;
                let catalog = match catalogs.entry(house) {
                    Entry::Occupied(known) => known.into_mut(),
                    Entry::Vacant(slot) => {
                        slot.insert(favorites::list(t, households, coordinator)?)
                    }
                };
                let found = favorite(catalog, name)?;
                if found.kind == FavoriteKind::Unplayable || found.uri.is_none() {
                    return Err(FavoriteError::Unplayable {
                        title: found.title.clone(),
                    }
                    .into());
                }
            }
            SceneOp::Uri {
                coordinator, uri, ..
            } if uri.starts_with("spotify:") => {
                control::check_spotify_source(t, households, coordinator, uri)?;
            }
            SceneOp::Tracks { coordinator, .. } => {
                control::spotify_params(t, households, coordinator)?
                    .ok_or(CoreError::NoSpotifyFavorite)?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn audience<'a>(
    households: &'a [HouseholdState],
    coordinator: &PlayerId,
) -> Result<Vec<&'a str>, CoreError> {
    let mut groups = households
        .iter()
        .flat_map(|house| &house.groups)
        .filter(|group| group.coordinator == *coordinator);
    let group = groups
        .next()
        .filter(|group| group.members.contains(coordinator))
        .ok_or_else(|| invalid_audience("the selected room no longer leads its confirmed group"))?;
    if groups.next().is_some() {
        return Err(invalid_audience("the selected room's group is ambiguous"));
    }
    let mut members: Vec<_> = group
        .members
        .iter()
        .map(|player| player.0.as_str())
        .collect();
    members.sort_unstable();
    members.dedup();
    Ok(members)
}

fn invalid_audience(detail: &str) -> CoreError {
    ProtoError::Malformed(format!("scene source refused: {detail}")).into()
}

/// A source load must retain the execution engine's confirmed audience,
/// including every member whose playback caps were checked before dispatch.
pub(super) fn confirm_audience<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = control::locate(households, coordinator)?.ip;
    let topology = read_at(transport, host, coordinator)?;
    if !crate::topology::coordinator_at(&topology, host, coordinator)? {
        return Err(invalid_audience(
            "the selected room no longer owns its control address; refresh and apply again",
        ));
    }
    let mut live = HouseholdState::default();
    live.apply_topology(&topology);
    if audience(households, coordinator)? != audience(&[live], coordinator)? {
        return Err(invalid_audience(
            "the group's audience changed after scene preparation; refresh and apply again",
        ));
    }
    Ok(())
}

/// Captured scenes store the exact title, including titles that look like a
/// position or native id. Preserve that precedence while refusing duplicate
/// titles; choosing the first would make the music depend on catalog order.
pub(super) fn favorite<'a>(
    catalog: &'a [Favorite],
    name: &str,
) -> Result<&'a Favorite, FavoriteError> {
    let exact: Vec<_> = catalog.iter().filter(|item| item.title == name).collect();
    match exact.as_slice() {
        [] => favorites::find(catalog, name),
        [found] => Ok(found),
        _ => Err(FavoriteError::Ambiguous {
            query: name.into(),
            candidates: exact.iter().map(|item| item.title.clone()).collect(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, title: &str) -> Favorite {
        Favorite {
            id: id.into(),
            title: title.into(),
            kind: FavoriteKind::Stream,
            uri: Some(format!("test:{id}")),
            metadata: String::new(),
            description: None,
            art_uri: None,
        }
    }

    #[test]
    fn captured_titles_take_precedence_over_positions_and_ids() {
        let catalog = [item("FV:2/1", "Station"), item("FV:2/2", "1")];
        assert_eq!(favorite(&catalog, "1").unwrap().id, "FV:2/2");
        assert_eq!(favorite(&catalog, "FV:2/1").unwrap().title, "Station");
        assert_eq!(favorite(&catalog, "sta").unwrap().id, "FV:2/1");
    }

    #[test]
    fn duplicate_exact_titles_do_not_choose_a_catalog_position() {
        let catalog = [item("FV:2/1", "Station"), item("FV:2/2", "Station")];
        assert!(matches!(
            favorite(&catalog, "Station"),
            Err(FavoriteError::Ambiguous { candidates, .. }) if candidates.len() == 2
        ));
        assert_eq!(favorite(&catalog, "FV:2/2").unwrap().id, "FV:2/2");
    }
}
