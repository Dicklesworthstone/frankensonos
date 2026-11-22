//! Copy observed native track occurrences into an existing saved playlist.
//! Source selections retain the speaker's metadata; requests never supply it.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistContentsRequest, SonosPlaylistDto, Surface};
use crate::failure::{ErrorCode, Failure};

const TOOL: &str = "append_sonos_playlist_tracks";

/// Append 1–10 occurrences in their supplied order. A missing source_playlist
/// selects the group's observed live queue; an SQ: source selects a saved
/// playlist in the same household. Each source has its own observed revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppendSonosPlaylistTracksRequest {
    pub zone: String,
    /// Exact destination SQ: id from sonos_playlist_contents.
    pub playlist: String,
    /// Destination playlist revision, independent of source_revision.
    pub expected_revision: String,
    /// Omit to select the live queue; otherwise supply an exact source SQ: id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_playlist: Option<String>,
    /// get_queue revision or the source saved playlist's contents revision.
    pub source_revision: String,
    /// One-based source occurrences, preserving order and deliberate repeats.
    pub positions: Vec<u32>,
}

impl AppendSonosPlaylistTracksRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        SonosPlaylistContentsRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            offset: 0,
            limit: 1,
            expected_revision: Some(self.expected_revision.clone()),
        }
        .validate()?;
        if self.source_revision.trim().is_empty()
            || self.source_revision.len() > 4096
            || self.source_revision.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "source_revision must be nonempty and at most 4096 bytes without control characters",
            ));
        }
        if let Some(playlist) = &self.source_playlist {
            SonosPlaylistContentsRequest {
                zone: self.zone.clone(),
                playlist: playlist.clone(),
                offset: 0,
                limit: 1,
                expected_revision: Some(self.source_revision.clone()),
            }
            .validate()?;
        }
        if self.positions.is_empty()
            || self.positions.len() > control::MAX_TRACK_BATCH
            || self.positions.iter().any(|position| {
                *position == 0 || *position > fsonos_proto::content::MAX_BROWSE_OBJECTS
            })
        {
            return Err(Failure::invalid(
                "select between 1 and 10 bounded one-based source occurrences",
            ));
        }
        Ok(())
    }
}

/// The complete destination was confirmed after every requested append.
/// A partial or uncertain write is returned as a Failure and never replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AppendedSonosPlaylistDto {
    pub done: String,
    pub zone: String,
    pub playlist: SonosPlaylistDto,
    pub revision: String,
    pub previous_total: u32,
    pub total: u32,
    pub added: u32,
    pub positions: Vec<u32>,
    pub titles: Vec<String>,
    pub logged: bool,
}

impl AppendedSonosPlaylistDto {
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("{}\nRevision: {}\n", self.done, self.revision);
        if !self.logged {
            text.push_str(
                "The saved-playlist append was confirmed, but the action log could not be written; do not repeat this operation.\n",
            );
        }
        text
    }
}

impl Surface {
    /// Append a captured source selection without changing the source queue,
    /// the destination playlist's identity, playback, or the current DJ.
    pub fn append_sonos_playlist_tracks(
        &self,
        client: &Client,
        req: &AppendSonosPlaylistTracksRequest,
    ) -> Result<AppendedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, TOOL)?;
        req.validate()?;
        let zone = req.zone.trim();
        let destination = req.playlist.trim();
        let source = req.source_playlist.as_deref().map(str::trim);
        let intent = format!(
            "{TOOL}: {zone} {} occurrences {:?} into {destination}",
            source.unwrap_or("queue"),
            req.positions,
        );
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<AppendedSonosPlaylistDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            let tracks = match source {
                Some(playlist) => playlists::select_tracks(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    playlist,
                    &req.positions,
                    &req.source_revision,
                )?,
                None => playlists::select_queue_tracks(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    &req.positions,
                    &req.source_revision,
                )?,
            };
            self.guard(client).authorize(TOOL, false)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let change = playlists::append_tracks(
                &*self.transport,
                &households,
                &group.coordinator,
                destination,
                &req.expected_revision,
                &tracks,
            )?;
            let added = u32::try_from(tracks.len()).expect("bounded native selection");
            Ok(AppendedSonosPlaylistDto {
                done: format!(
                    "added {added} selected tracks to Sonos playlist {:?} ({}) on {}'s household; {} entries total",
                    change.playlist.title, change.playlist.id, player.room_name, change.total,
                ),
                zone: player.room_name.clone(),
                playlist: SonosPlaylistDto::from(&change.playlist),
                revision: change.revision,
                previous_total: change.previous_total,
                total: change.total,
                added,
                positions: req.positions.clone(),
                titles: tracks.into_iter().map(|track| track.title).collect(),
                logged: false,
            })
        })();
        match result {
            Ok(mut appended) => {
                appended.logged =
                    self.record(client, intent, "allow".into(), appended.done.clone(), None);
                Ok(appended)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint(
                        "Read the destination saved playlist on the original household before another append; some selected occurrences may already be present and the operation was not replayed.",
                    );
                }
                self.notice(&failure);
                let decision = if failure.code == ErrorCode::PolicyDenied {
                    format!("deny: {}", failure.detail)
                } else {
                    "allow".into()
                };
                self.record(
                    client,
                    intent,
                    decision,
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
