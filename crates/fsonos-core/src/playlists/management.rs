//! Native catalog mutations address one observed identity. They do not save
//! over a playlist, rebuild the queue, or recover a write by sending it again.

use super::{GroupProof, contents, invalid, list, native_id, validate_title};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, control};
use fsonos_proto::{ProtoError, Transport, content};
use fsonos_types::PlayerId;
use std::net::IpAddr;

/// A confirmed saved-playlist catalog change. Removal returns the entry that
/// was observed before deletion; rename returns its confirmed new metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistChange {
    pub playlist: Favorite,
    pub changed: bool,
}

/// Rename exactly one native saved playlist, retaining its identity and tracks.
///
/// The caller must supply the exact `SQ:<n>` id and revision from [`contents`].
/// Its title, contents, household, coordinator, boot and group are checked
/// again before one native `UpdateObject`. The current title is also supplied
/// to the speaker as its update precondition. An already matching trimmed
/// title is a confirmed no-op, after the same freshness checks.
///
/// A lost/malformed acknowledgement or unconfirmed catalog read-back returns
/// [`CoreError::Uncertain`]; it never replays the mutation. These observations
/// cannot lock out another controller between the last read and native write.
/// Queue contents, playback, volume and DJ ownership are left alone.
pub fn rename<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
    title: &str,
) -> Result<PlaylistChange, CoreError> {
    let title = validate_title(title).map_err(invalid)?;
    let selected = Selected::read(
        transport,
        households,
        coordinator,
        playlist_id,
        expected_revision,
    )?;
    if selected.playlist.title == title {
        return Ok(PlaylistChange {
            playlist: selected.playlist,
            changed: false,
        });
    }
    acknowledged(
        content::rename_object(
            transport,
            selected.host,
            playlist_id,
            &selected.playlist.title,
            title,
        ),
        "rename",
    )?;
    let renamed = selected
        .catalog(transport, households, coordinator, "rename")?
        .into_iter()
        .find(|item| {
            item.id == playlist_id && item.title == title && item.uri == selected.playlist.uri
        })
        .ok_or_else(|| {
            uncertain(
                "rename",
                "the saved catalog did not confirm the same playlist with its new title",
            )
        })?;
    Ok(PlaylistChange {
        playlist: renamed,
        changed: true,
    })
}

/// Remove exactly one observed native saved playlist from its household.
///
/// A mandatory [`contents`] revision binds the id to the title, contents and
/// player the caller observed. Fresh reads precede a single `DestroyObject`;
/// success requires a complete catalog read confirming that id is absent.
/// A lost/malformed acknowledgement or failed read-back is uncertain and is
/// never retried. Sonos provides no revision argument for `DestroyObject`, so
/// a different controller can still race the last observation.
///
/// This does not remove queued copies of its tracks, interrupt playback, or
/// alter other playlists with the same title. The result carries the removed
/// entry so callers can report the exact native identity that was affected.
pub fn remove<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    playlist_id: &str,
    expected_revision: &str,
) -> Result<PlaylistChange, CoreError> {
    let selected = Selected::read(
        transport,
        households,
        coordinator,
        playlist_id,
        expected_revision,
    )?;
    acknowledged(
        content::destroy_object(transport, selected.host, playlist_id),
        "removal",
    )?;
    if selected
        .catalog(transport, households, coordinator, "removal")?
        .iter()
        .any(|item| item.id == playlist_id)
    {
        return Err(uncertain(
            "removal",
            "the selected playlist is still present in the saved catalog",
        ));
    }
    Ok(PlaylistChange {
        playlist: selected.playlist,
        changed: true,
    })
}

struct Selected {
    host: IpAddr,
    group: GroupProof,
    playlist: Favorite,
}

impl Selected {
    fn read<T: Transport + ?Sized>(
        transport: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        playlist_id: &str,
        expected_revision: &str,
    ) -> Result<Self, CoreError> {
        if !native_id(playlist_id) || playlist_id.len() > 1024 {
            return Err(invalid(
                "changing a saved playlist requires an exact native SQ: id",
            ));
        }
        if expected_revision.trim().is_empty()
            || expected_revision.len() > 4096
            || expected_revision.chars().any(char::is_control)
        {
            return Err(invalid(
                "changing a saved playlist requires its observed revision",
            ));
        }
        let host = control::locate(households, coordinator)?.ip;
        let group = GroupProof::read(transport, host, coordinator)?;
        let observed = contents(
            transport,
            households,
            coordinator,
            playlist_id,
            0,
            1,
            Some(expected_revision),
        )?;
        group.confirm(transport, host, coordinator)?;
        Ok(Self {
            host,
            group,
            playlist: observed.playlist,
        })
    }

    fn catalog<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        operation: &str,
    ) -> Result<Vec<Favorite>, CoreError> {
        self.group
            .confirm(transport, self.host, coordinator)
            .map_err(|error| uncertain(operation, error))?;
        let catalog = list(transport, households, coordinator)
            .map_err(|error| uncertain(operation, error))?;
        self.group
            .confirm(transport, self.host, coordinator)
            .map_err(|error| uncertain(operation, error))?;
        Ok(catalog)
    }
}

fn acknowledged(result: Result<(), ProtoError>, operation: &str) -> Result<(), CoreError> {
    match result {
        Ok(()) => Ok(()),
        Err(error @ ProtoError::SoapFault { .. }) => Err(error.into()),
        Err(error) => Err(uncertain(operation, error)),
    }
}

fn uncertain(operation: &str, detail: impl std::fmt::Display) -> CoreError {
    CoreError::Uncertain(format!(
        "Sonos playlist {operation} could not be confirmed: {detail}; the saved catalog may \
         have changed; refresh the saved-playlist catalog before trying again"
    ))
}
