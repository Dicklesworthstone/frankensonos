//! Household-bound native playlist sources. A scene remembers an identity,
//! not a renderer URI or a frozen copy of the playlist's contents.

use super::{SceneHooks, SceneOp, ScenePlan};
use crate::favorites::{self, Favorite};
use crate::{CoreError, HouseholdState, control, playlists};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{HouseholdId, PlayerId};
use serde::{Deserialize, Serialize};

/// A playlist selected from one household's native `SQ:` catalog. Recall
/// uses its current contents, but a changed title requires saving the scene
/// again. Equal native IDs in different households never identify the same music.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SonosPlaylistReference {
    pub household: HouseholdId,
    pub id: String,
    pub title: String,
}

fn invalid(detail: impl std::fmt::Display) -> CoreError {
    ProtoError::Malformed(format!("saved Sonos playlist scene refused: {detail}")).into()
}

fn household<'a>(
    households: &'a [HouseholdState],
    player: &PlayerId,
) -> Result<&'a HouseholdId, CoreError> {
    let mut owners = households.iter().filter(|h| h.player(player).is_some());
    let owner = owners
        .next()
        .ok_or_else(|| CoreError::UnknownPlayer(player.0.clone()))?;
    if owners.next().is_some() {
        return Err(invalid("the selected player's household is ambiguous"));
    }
    owner
        .id
        .as_ref()
        .filter(|id| !id.0.trim().is_empty())
        .ok_or_else(|| invalid("the selected household has no confirmed identity"))
}

fn catalog_entry<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    player: &PlayerId,
    id: &str,
) -> Result<Favorite, CoreError> {
    let host = control::locate(households, player)?.ip;
    // The prospective coordinator may currently be a group member. Prove
    // its address/identity without requiring a regroup during this read.
    let identity = || -> Result<Option<u32>, CoreError> {
        let state = get_zone_group_state(t, host)?;
        let mut members = state
            .groups
            .iter()
            .flat_map(|g| &g.members)
            .filter(|m| m.uuid == *player && m.ip() == Some(host) && !m.is_zone_bridge);
        let member = members
            .next()
            .ok_or_else(|| invalid("the selected player's address changed; refresh the house"))?;
        if members.next().is_some() {
            return Err(invalid("the selected player's identity is ambiguous"));
        }
        Ok(member.boot_seq)
    };
    let before = identity()?;
    let found = playlists::list(t, households, player)?
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| {
            invalid(format!(
                "playlist {id} is no longer in its household; save the scene again"
            ))
        })?;
    if identity()? != before {
        return Err(invalid(
            "the selected player restarted during the catalog read",
        ));
    }
    Ok(found)
}

impl SonosPlaylistReference {
    pub(super) fn validate(&self) -> Result<(), String> {
        if self.household.0.trim().is_empty()
            || self.household.0.len() > 1024
            || self.household.0.chars().any(char::is_control)
            || self.id.len() > 1024
            || !playlists::native_id(&self.id)
            || playlists::validate_title(&self.title).is_err()
        {
            return Err(
                "a native playlist scene needs its household, SQ: id and catalog title".into(),
            );
        }
        Ok(())
    }

    /// Pin a native playlist from the selected player's household. This
    /// operation only reads the catalog; no queue or speaker setting changes.
    pub fn capture<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        player: &PlayerId,
        id: &str,
    ) -> Result<Self, CoreError> {
        if id.len() > 1024 || !playlists::native_id(id) {
            return Err(invalid("select an exact native SQ: playlist id"));
        }
        let owner = household(households, player)?.clone();
        let item = catalog_entry(t, households, player, id)?;
        let reference = Self {
            household: owner,
            id: item.id,
            title: item.title,
        };
        reference.validate().map_err(invalid)?;
        Ok(reference)
    }

    pub(super) fn check_household(
        &self,
        households: &[HouseholdState],
        player: &PlayerId,
    ) -> Result<(), CoreError> {
        self.validate().map_err(invalid)?;
        if household(households, player)? != &self.household {
            return Err(invalid(
                "the target room is no longer in the playlist's saved household; save the scene again",
            ));
        }
        Ok(())
    }

    fn resolve<T: Transport + ?Sized>(
        &self,
        t: &T,
        households: &[HouseholdState],
        player: &PlayerId,
    ) -> Result<Favorite, CoreError> {
        self.check_household(households, player)?;
        let item = catalog_entry(t, households, player, &self.id)?;
        if item.title != self.title {
            return Err(invalid(format!(
                "playlist {} was renamed or replaced; save the scene again",
                self.id
            )));
        }
        Ok(item)
    }
}

/// Read every permitted native playlist source before a surface changes
/// caps, grouping or playback. Execution resolves again immediately before
/// loading; these observations do not lock the speaker's catalog.
pub fn check_playlist_sources<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    plan: &ScenePlan,
) -> Result<(), CoreError> {
    for op in &plan.ops {
        if let SceneOp::SonosPlaylist {
            coordinator,
            playlist,
            ..
        } = op
        {
            playlist.resolve(t, households, coordinator)?;
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
        .ok_or_else(|| invalid("the selected room no longer leads its confirmed group"))?;
    if groups.next().is_some() {
        return Err(invalid("the selected room's group is ambiguous"));
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

fn confirm_audience<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
) -> Result<(), CoreError> {
    let host = control::locate(households, coordinator)?.ip;
    let topology = get_zone_group_state(t, host)?;
    let mut live = HouseholdState::default();
    live.apply_topology(&topology);
    if audience(households, coordinator)? != audience(&[live], coordinator)? {
        return Err(invalid(
            "the group's audience changed after scene preparation; refresh and apply again",
        ));
    }
    Ok(())
}

pub(super) fn recall<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist: &SonosPlaylistReference,
    start: bool,
    hooks: &dyn SceneHooks,
) -> Result<(), String> {
    let current = playlist
        .resolve(t, households, coordinator)
        .map_err(|e| e.to_string())?;
    // Preflight permits a room that will become coordinator after regrouping.
    // Dispatch must instead retain the execution engine's confirmed audience,
    // including every room covered by its playback volume caps.
    confirm_audience(t, households, coordinator).map_err(|e| e.to_string())?;
    if hooks.queue_replacement_started(coordinator) {
        let receipt = if start {
            favorites::play_with_receipt(t, households, coordinator, &current)
        } else {
            favorites::load_with_receipt(t, households, coordinator, &current)
        }
        .map_err(|e| e.to_string())?;
        if let Some(receipt) = receipt {
            hooks.queue_replacement_finished(coordinator, receipt);
        }
        return Ok(());
    }
    let result = if start {
        favorites::play(t, households, coordinator, &current)
    } else {
        favorites::load(t, households, coordinator, &current)
    };
    result.map_err(|e| e.to_string())
}
