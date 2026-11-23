//! Add an explicit Spotify selection to a saved Sonos playlist without
//! staging it in the playing queue. Renderer metadata is learned locally.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistContentsRequest, SonosPlaylistDto, Surface};
use crate::execute::spotify_failure;
use crate::failure::{ErrorCode, Failure};
use crate::surface::queue::{PlayTracksRequest, QueueTrackInput};

const TOOL: &str = "add_tracks_to_sonos_playlist";

/// Add 1–10 concrete track or episode occurrences to an existing native
/// playlist. The destination revision is independent of any library page.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AddTracksToSonosPlaylistRequest {
    pub zone: String,
    /// Exact destination SQ: id from sonos_playlist_contents.
    pub playlist: String,
    /// The destination's observed saved-contents revision.
    pub expected_revision: String,
    /// Input order and deliberate repetitions are retained.
    pub tracks: Vec<QueueTrackInput>,
}

impl AddTracksToSonosPlaylistRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        self.normalized().map(|_| ())
    }

    fn normalized(&self) -> Result<Self, Failure> {
        SonosPlaylistContentsRequest {
            zone: self.zone.clone(),
            playlist: self.playlist.clone(),
            offset: 0,
            limit: 1,
            expected_revision: Some(self.expected_revision.clone()),
        }
        .validate()?;
        // Share source/link/title validation with ordered playback. Its
        // destination queue token is unrelated to this saved playlist token.
        let selection = PlayTracksRequest {
            zone: self.zone.clone(),
            tracks: self.tracks.clone(),
            expected_revision: None,
        }
        .normalized()?;
        Ok(Self {
            zone: selection.zone,
            playlist: self.playlist.trim().into(),
            expected_revision: self.expected_revision.clone(),
            tracks: selection.tracks,
        })
    }
}

/// Complete saved contents were confirmed after every requested addition.
/// A partial or uncertain batch is a Failure and must not be replayed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct AddedTracksToSonosPlaylistDto {
    pub done: String,
    pub zone: String,
    pub playlist: SonosPlaylistDto,
    pub revision: String,
    pub previous_total: u32,
    pub total: u32,
    pub added: u32,
    pub source_uris: Vec<String>,
    pub titles: Vec<String>,
    pub logged: bool,
}

impl AddedTracksToSonosPlaylistDto {
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!("{}\nRevision: {}\n", self.done, self.revision);
        if !self.logged {
            text.push_str(
                "The saved-playlist additions were confirmed, but the action log could not be written; do not repeat this operation.\n",
            );
        }
        text
    }
}

impl Surface {
    /// Render explicit identities for this household and add them to its
    /// reviewed playlist. No Web API grant, playback permission or queue
    /// staging is needed; this is a house-config operation.
    pub fn add_tracks_to_sonos_playlist(
        &self,
        client: &Client,
        req: &AddTracksToSonosPlaylistRequest,
    ) -> Result<AddedTracksToSonosPlaylistDto, Failure> {
        self.authorize_logged(client, TOOL)?;
        let asked = req.normalized()?;
        let intent = format!(
            "{TOOL}: {} {} selected tracks into {}",
            asked.zone,
            asked.tracks.len(),
            asked.playlist,
        );
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<AddedTracksToSonosPlaylistDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, &asked.zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            let selection: Vec<_> = asked
                .tracks
                .iter()
                .map(|track| {
                    (
                        track.source_uri.as_str(),
                        track.title.as_deref().unwrap_or(&track.source_uri),
                    )
                })
                .collect();
            let tracks = playlists::prepare_spotify_tracks(
                &*self.transport,
                &households,
                &group.coordinator,
                &selection,
            )
            .map_err(spotify_failure)?;
            // Renderer learning can span network reads. Retain the original
            // audience and apply the caller's current policy before any write.
            self.guard(client).authorize(TOOL, false)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let change = playlists::append_tracks(
                &*self.transport,
                &households,
                &group.coordinator,
                &asked.playlist,
                &asked.expected_revision,
                &tracks,
            )?;
            let added = u32::try_from(tracks.len()).expect("bounded Spotify selection");
            Ok(AddedTracksToSonosPlaylistDto {
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
                source_uris: asked
                    .tracks
                    .iter()
                    .map(|track| track.source_uri.clone())
                    .collect(),
                titles: tracks.into_iter().map(|track| track.title).collect(),
                logged: false,
            })
        })();
        match result {
            Ok(mut added) => {
                added.logged =
                    self.record(client, intent, "allow".into(), added.done.clone(), None);
                Ok(added)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint(
                        "Read the destination saved playlist on the original household before another addition; some selected tracks may already be present and the operation was not replayed.",
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
