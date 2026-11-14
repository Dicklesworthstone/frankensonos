//! Revision-bound edits to native saved playlists. These catalog operations do
//! not touch playback and must never be replayed through transport healing.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistContentsRequest, SonosPlaylistDto, Surface};
use crate::failure::{ErrorCode, Failure};

/// Rename the exact playlist whose current contents revision was reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameSonosPlaylistRequest {
    pub zone: String,
    /// The exact household-local SQ: id, never a title or catalog position.
    pub playlist: String,
    /// The opaque revision returned by sonos_playlist_contents.
    pub expected_revision: String,
    /// A nonblank title, at most 1023 UTF-8 bytes after trimming.
    pub title: String,
}

impl RenameSonosPlaylistRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate(&self.zone, &self.playlist, &self.expected_revision)?;
        playlists::validate_title(&self.title).map_err(Failure::invalid)?;
        Ok(())
    }
}

/// Permanently remove one reviewed native saved playlist from its household.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RemoveSonosPlaylistRequest {
    pub zone: String,
    pub playlist: String,
    /// The opaque revision returned by sonos_playlist_contents.
    pub expected_revision: String,
}

impl RemoveSonosPlaylistRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        validate(&self.zone, &self.playlist, &self.expected_revision)
    }
}

fn validate(zone: &str, playlist: &str, revision: &str) -> Result<(), Failure> {
    SonosPlaylistContentsRequest {
        zone: zone.into(),
        playlist: playlist.into(),
        offset: 0,
        limit: 1,
        expected_revision: Some(revision.into()),
    }
    .validate()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SonosPlaylistOperation {
    Rename,
    Remove,
}

/// A native catalog postcondition confirmed on the original household.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ManagedSonosPlaylistDto {
    pub done: String,
    pub zone: String,
    /// The confirmed renamed entry, or the observed entry that was removed.
    pub playlist: SonosPlaylistDto,
    pub operation: SonosPlaylistOperation,
    /// False when a reviewed rename already has the requested title.
    pub changed: bool,
    /// A logging failure does not invalidate the confirmed native result.
    pub logged: bool,
}

impl ManagedSonosPlaylistDto {
    #[must_use]
    pub fn text(&self) -> String {
        if self.logged {
            format!("{}\n", self.done)
        } else {
            format!(
                "{}\nThe catalog result was confirmed, but the action log could not be written; do not repeat this operation.\n",
                self.done
            )
        }
    }
}

impl Surface {
    pub fn rename_sonos_playlist(
        &self,
        client: &Client,
        req: &RenameSonosPlaylistRequest,
    ) -> Result<ManagedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, "rename_sonos_playlist")?;
        req.validate()?;
        self.manage_sonos_playlist(
            client,
            req.zone.trim(),
            req.playlist.trim(),
            &req.expected_revision,
            Some(req.title.trim()),
        )
    }

    pub fn remove_sonos_playlist(
        &self,
        client: &Client,
        req: &RemoveSonosPlaylistRequest,
    ) -> Result<ManagedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, "remove_sonos_playlist")?;
        req.validate()?;
        self.manage_sonos_playlist(
            client,
            req.zone.trim(),
            req.playlist.trim(),
            &req.expected_revision,
            None,
        )
    }

    fn manage_sonos_playlist(
        &self,
        client: &Client,
        zone: &str,
        playlist_id: &str,
        revision: &str,
        title: Option<&str>,
    ) -> Result<ManagedSonosPlaylistDto, Failure> {
        let (operation, intent) = match title {
            Some(title) => (
                SonosPlaylistOperation::Rename,
                format!("rename_sonos_playlist: {zone} {playlist_id} {title:?}"),
            ),
            None => (
                SonosPlaylistOperation::Remove,
                format!("remove_sonos_playlist: {zone} {playlist_id}"),
            ),
        };
        let _changing = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<ManagedSonosPlaylistDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let change = match title {
                Some(title) => playlists::rename(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    playlist_id,
                    revision,
                    title,
                )?,
                None => playlists::remove(
                    &*self.transport,
                    &households,
                    &group.coordinator,
                    playlist_id,
                    revision,
                )?,
            };
            let action = match (operation, change.changed) {
                (SonosPlaylistOperation::Rename, false) => "already named",
                (SonosPlaylistOperation::Rename, true) => "renamed",
                (SonosPlaylistOperation::Remove, _) => "removed",
            };
            Ok(ManagedSonosPlaylistDto {
                done: format!(
                    "{action} Sonos playlist {:?} ({}) in {}'s household",
                    change.playlist.title, change.playlist.id, player.room_name
                ),
                zone: player.room_name.clone(),
                playlist: SonosPlaylistDto::from(&change.playlist),
                operation,
                changed: change.changed,
                logged: false,
            })
        })();
        match result {
            Ok(mut managed) => {
                managed.logged =
                    self.record(client, intent, "allow".into(), managed.done.clone(), None);
                Ok(managed)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint(
                        "Read the original household's saved Sonos playlists before another catalog edit; the operation was not automatically retried.",
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
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn catalog_writes_require_exact_native_identity_and_a_contents_revision() {
        let request = |playlist: &str, revision: &str| RemoveSonosPlaylistRequest {
            zone: "Den@S1".into(),
            playlist: playlist.into(),
            expected_revision: revision.into(),
        };
        request("SQ:7", "opaque household revision")
            .validate()
            .unwrap();
        for playlist in ["Evening", "7", "SQ:", "SQ:7/3", "FV:2/7", "SQ:-1"] {
            assert!(request(playlist, "revision").validate().is_err());
        }
        for revision in ["", "  ", "a\nb"] {
            assert!(request("SQ:7", revision).validate().is_err());
        }
        assert!(request("SQ:7", &"x".repeat(4097)).validate().is_err());
        for body in [
            json!({"zone":"Den", "playlist":"SQ:7"}),
            json!({"zone":"Den", "playlist":"SQ:7", "expected_revision":"r", "force":true}),
        ] {
            assert!(serde_json::from_value::<RemoveSonosPlaylistRequest>(body).is_err());
        }
        for title in [String::new(), "a\nb".into(), "é".repeat(512)] {
            assert!(
                RenameSonosPlaylistRequest {
                    zone: "Den".into(),
                    playlist: "SQ:7".into(),
                    expected_revision: "r".into(),
                    title,
                }
                .validate()
                .is_err()
            );
        }
    }
}
