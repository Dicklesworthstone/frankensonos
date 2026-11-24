//! Native saved-playlist occurrence edits. The complete catalog postcondition
//! is verified by core; these operations never edit the playing queue.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistContentsRequest, SonosPlaylistDto, Surface};
use crate::failure::{ErrorCode, Failure};

/// Permanently remove a contiguous block from a reviewed native saved playlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveSonosPlaylistTracksRequest {
    pub zone: String,
    /// Exact household-local SQ: id from sonos_playlist_contents.
    pub playlist: String,
    pub expected_revision: String,
    /// First selected occurrence in the original playlist, starting at one.
    pub position: u32,
    /// Number of consecutive occurrences to remove.
    pub count: u32,
}

impl RemoveSonosPlaylistTracksRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate(
            &self.zone,
            &self.playlist,
            &self.expected_revision,
            self.position,
            self.count,
        )
    }
}

/// Move a contiguous block within a saved playlist, preserving its order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MoveSonosPlaylistTracksRequest {
    pub zone: String,
    /// Exact household-local SQ: id from sonos_playlist_contents.
    pub playlist: String,
    pub expected_revision: String,
    /// First selected occurrence in the original playlist, starting at one.
    pub position: u32,
    pub count: u32,
    /// Insert before this ORIGINAL one-based position. Original total + 1
    /// appends. A slot in the block or immediately after it is a verified no-op.
    pub insert_before: u32,
}

impl MoveSonosPlaylistTracksRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate(
            &self.zone,
            &self.playlist,
            &self.expected_revision,
            self.position,
            self.count,
        )?;
        if self.insert_before == 0 {
            return Err(Failure::invalid("insert_before must start at one"));
        }
        Ok(())
    }
}

fn validate(
    zone: &str,
    playlist: &str,
    revision: &str,
    position: u32,
    count: u32,
) -> Result<(), Failure> {
    SonosPlaylistContentsRequest {
        zone: zone.into(),
        playlist: playlist.into(),
        offset: 0,
        limit: 1,
        expected_revision: Some(revision.into()),
    }
    .validate()?;
    if position == 0
        || !(1..=playlists::MAX_PLAYLIST_EDIT_TRACKS).contains(&count)
        || position.checked_add(count - 1).is_none()
    {
        return Err(Failure::invalid(format!(
            "give a one-based position and 1 to {} consecutive saved-playlist entries without overflow",
            playlists::MAX_PLAYLIST_EDIT_TRACKS
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SonosPlaylistEditOperation {
    RemoveTracks,
    MoveTracks,
}

impl fastapi_openapi::JsonSchema for SonosPlaylistEditOperation {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::string_enum(
            ["remove_tracks", "move_tracks"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    fn schema_name() -> Option<&'static str> {
        Some("SonosPlaylistEditOperation")
    }
}

/// A verified native edit, with a new revision for an explicit subsequent read
/// or edit. A lost write response is uncertain, never automatically replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EditedSonosPlaylistDto {
    pub done: String,
    pub zone: String,
    pub playlist: SonosPlaylistDto,
    pub operation: SonosPlaylistEditOperation,
    /// The original first selected position, as supplied in the request.
    pub position: u32,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub insert_before: Option<u32>,
    pub previous_total: u32,
    pub total: u32,
    pub revision: String,
    pub changed: bool,
    /// A failed local log write does not undo the confirmed speaker edit.
    pub logged: bool,
}

impl EditedSonosPlaylistDto {
    fn confirmed(
        req: &RemoveSonosPlaylistTracksRequest,
        insert_before: Option<u32>,
        change: playlists::PlaylistTrackChange,
        room: &str,
    ) -> Self {
        let end = req.position + (req.count - 1);
        let (operation, effect) = match insert_before {
            Some(at) if change.changed => (
                SonosPlaylistEditOperation::MoveTracks,
                format!(
                    "moved saved entries {} through {end} before original position {at}",
                    req.position
                ),
            ),
            Some(_) => (
                SonosPlaylistEditOperation::MoveTracks,
                format!(
                    "saved entries {} through {end} already occupy that slot",
                    req.position
                ),
            ),
            None => (
                SonosPlaylistEditOperation::RemoveTracks,
                format!("removed saved entries {} through {end}", req.position),
            ),
        };
        Self {
            done: format!(
                "{effect} in Sonos playlist {:?} ({}) on {room}'s household; {} entries remain",
                change.playlist.title, change.playlist.id, change.total
            ),
            zone: room.into(),
            playlist: SonosPlaylistDto::from(&change.playlist),
            operation,
            position: req.position,
            count: req.count,
            insert_before,
            previous_total: change.previous_total,
            total: change.total,
            revision: change.revision,
            changed: change.changed,
            logged: false,
        }
    }

    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("{}\nRevision: {}\n", self.done, self.revision);
        if !self.logged {
            text.push_str(
                "The saved-playlist result was confirmed, but the action log could not be written; do not repeat this operation.\n",
            );
        }
        text
    }
}

impl Surface {
    pub fn remove_sonos_playlist_tracks(
        &self,
        client: &Client,
        req: &RemoveSonosPlaylistTracksRequest,
    ) -> Result<EditedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, "remove_sonos_playlist_tracks")?;
        req.validate()?;
        self.edit_sonos_playlist(client, req, None)
    }

    pub fn move_sonos_playlist_tracks(
        &self,
        client: &Client,
        req: &MoveSonosPlaylistTracksRequest,
    ) -> Result<EditedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, "move_sonos_playlist_tracks")?;
        req.validate()?;
        self.edit_sonos_playlist(
            client,
            &RemoveSonosPlaylistTracksRequest {
                zone: req.zone.clone(),
                playlist: req.playlist.clone(),
                expected_revision: req.expected_revision.clone(),
                position: req.position,
                count: req.count,
            },
            Some(req.insert_before),
        )
    }

    fn edit_sonos_playlist(
        &self,
        client: &Client,
        req: &RemoveSonosPlaylistTracksRequest,
        insert_before: Option<u32>,
    ) -> Result<EditedSonosPlaylistDto, Failure> {
        let tool = if insert_before.is_some() {
            "move_sonos_playlist_tracks"
        } else {
            "remove_sonos_playlist_tracks"
        };
        let zone = req.zone.trim();
        let playlist_id = req.playlist.trim();
        let end = req.position + (req.count - 1);
        let destination = insert_before.map_or_else(String::new, |at| format!(" before {at}"));
        let intent = format!(
            "{tool}: {zone} {playlist_id} {}..={end}{destination}",
            req.position
        );
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<EditedSonosPlaylistDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            self.queue_group_unchanged(player.ip, &group)?;
            self.guard(client).authorize(tool, false)?;
            let change = match insert_before {
                Some(at) => playlists::move_tracks(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    playlist_id,
                    &req.expected_revision,
                    req.position,
                    req.count,
                    at,
                )?,
                None => playlists::remove_tracks(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    playlist_id,
                    &req.expected_revision,
                    req.position,
                    req.count,
                )?,
            };
            Ok(EditedSonosPlaylistDto::confirmed(
                req,
                insert_before,
                change,
                &player.room_name,
            ))
        })();
        match result {
            Ok(mut edited) => {
                edited.logged =
                    self.record(client, intent, "allow".into(), edited.done.clone(), None);
                Ok(edited)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint(
                        "Read this saved playlist's contents on the original household before another edit; the operation was not automatically retried.",
                    );
                }
                self.notice(&failure);
                self.record(
                    client,
                    intent,
                    "allow".into(),
                    format!("failed: {}", failure.detail),
                    None,
                );
                Err(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests;
