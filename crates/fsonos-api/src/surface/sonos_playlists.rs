//! Saved Sonos queues (`SQ:`), independent of My Sonos favorites and the
//! Spotify library. Resolve names only against the target household's current
//! list, then use the existing native-container playback and policy boundary.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::favorites::{self, Favorite, FavoriteError};
use fsonos_core::policy::Client;
use fsonos_core::{control, playlists};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, resolve, room_view};
use crate::OutcomeDto;
use crate::failure::{ErrorCode, Failure};
use crate::plan::Command;
use crate::request::zone_name;

mod contents;
pub use contents::{SonosPlaylistContentsDto, SonosPlaylistContentsRequest, SonosPlaylistItemDto};

mod track;
pub use track::{SonosPlaylistTrackDto, SonosPlaylistTrackMode, SonosPlaylistTrackRequest};

/// Saving a queue changes the household's catalog, not playback.
pub const SAVE: &str = "save_sonos_playlist";

/// A playlist saved on the Sonos household, rather than in a Spotify account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SonosPlaylistDto {
    /// The native `SQ:<n>` id, scoped to this household.
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_uri: Option<String>,
}

impl From<&Favorite> for SonosPlaylistDto {
    fn from(playlist: &Favorite) -> Self {
        Self {
            id: playlist.id.clone(),
            title: playlist.title.clone(),
            art_uri: playlist.art_uri.clone(),
        }
    }
}

/// Numbered titles and native ids, shared by CLI and MCP.
#[must_use]
pub fn text(playlists: &[SonosPlaylistDto]) -> String {
    if playlists.is_empty() {
        return "no saved Sonos playlists in this household\n".into();
    }
    playlists
        .iter()
        .enumerate()
        .fold(String::new(), |mut out, (i, playlist)| {
            let _ = writeln!(out, "{:>3}. {}  ({})", i + 1, playlist.title, playlist.id);
            out
        })
}

/// Replace the room's queue with a saved Sonos playlist and start it.
/// Only a name/id is accepted; the speaker supplies the URI and metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaySonosPlaylistRequest {
    pub zone: String,
    /// Native id, current numbered position, or an unambiguous title.
    pub playlist: String,
}

impl PlaySonosPlaylistRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        playlist_name(&self.playlist)?;
        Ok(())
    }
}

/// Save the group's current queue as a new native Sonos playlist.
/// An existing playlist id or replacement mode is deliberately not accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SaveSonosPlaylistRequest {
    pub zone: String,
    /// A nonblank title, at most 1023 UTF-8 bytes after trimming.
    pub title: String,
}

impl SaveSonosPlaylistRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        playlists::validate_title(&self.title).map_err(Failure::invalid)?;
        Ok(())
    }
}

/// A new playlist confirmed in the selected household's saved catalog.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SavedSonosPlaylistDto {
    pub done: String,
    /// The group whose queue was saved, named by its coordinator's room.
    pub zone: String,
    pub playlist: SonosPlaylistDto,
    /// Whether the separate local action log was written. The playlist itself
    /// is saved on Sonos even if this is false; do not repeat the save.
    pub logged: bool,
}

impl SavedSonosPlaylistDto {
    #[must_use]
    pub fn text(&self) -> String {
        if self.logged {
            format!("{}\n", self.done)
        } else {
            format!(
                "{}\nThe playlist was saved, but the action log could not be written; do not repeat the save.\n",
                self.done
            )
        }
    }
}

pub(crate) fn playlist_name(query: &str) -> Result<&str, Failure> {
    let query = query.trim();
    if query.is_empty() || query.chars().count() > 1024 || query.chars().any(char::is_control) {
        return Err(Failure::invalid(
            "playlist must be nonempty and at most 1024 characters without control characters",
        ));
    }
    Ok(query)
}

/// Use the same exact-id/title resolution as favorites, with errors that name
/// the native playlist catalog the caller actually selected from.
pub(crate) fn find<'a>(list: &'a [Favorite], query: &str) -> Result<&'a Favorite, Failure> {
    favorites::find(list, query).map_err(|error| match error {
        FavoriteError::Unknown { query, suggestions } => Failure::new(
            ErrorCode::NoMatch,
            format!("no saved Sonos playlist matches {query:?} in this household"),
        )
        .with_suggestions(suggestions)
        .with_hint("Read list_sonos_playlists for this room and choose its native id or title."),
        FavoriteError::Ambiguous { query, candidates } => Failure::invalid(format!(
            "{query:?} matches several saved Sonos playlists in this household",
        ))
        .with_suggestions(candidates)
        .with_hint("Read list_sonos_playlists and use the selected playlist's SQ: id."),
        other => Failure::from(other),
    })
}

impl Surface {
    /// Read native saved playlists under the existing `list_favorites`
    /// permission, without Spotify authentication or playback writes.
    pub fn sonos_playlists(
        &self,
        client: &Client,
        zone: &str,
    ) -> Result<Vec<SonosPlaylistDto>, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        zone_name("zone", zone)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|failure| self.explain(failure))?;
        let listed = playlists::list(&*self.transport, &households, &target.coordinator.id)?;
        Ok(listed.iter().map(SonosPlaylistDto::from).collect())
    }

    /// Play a native saved playlist through the same caps, action log, DJ
    /// handoff and staged queue replacement as a container favorite.
    pub fn play_sonos_playlist(
        &self,
        client: &Client,
        req: &PlaySonosPlaylistRequest,
    ) -> Result<OutcomeDto, Failure> {
        req.validate()?;
        self.control_io(client, "play_favorite", |transport, rooms| {
            let target = resolve(rooms, req.zone.trim())?;
            let listed = playlists::list(transport, rooms.households, &target.coordinator.id)?;
            let playlist = find(&listed, req.playlist.trim())?;
            Ok(Command::PlayFavorite {
                coordinator: target.coordinator.id.clone(),
                favorite: playlist.clone(),
            })
        })
    }

    /// Create a saved queue once, with fresh coordinator resolution and the
    /// catalog-write permission. It neither takes over the DJ nor changes any
    /// playback state, and does not claim an action-history undo operation.
    pub fn save_sonos_playlist(
        &self,
        client: &Client,
        req: &SaveSonosPlaylistRequest,
    ) -> Result<SavedSonosPlaylistDto, Failure> {
        self.authorize_logged(client, SAVE)?;
        req.validate()?;
        let zone = req.zone.trim();
        let title = req.title.trim();
        let intent = format!("{SAVE}: {zone} {title:?}");
        let _saving = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result: Result<SavedSonosPlaylistDto, Failure> = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let player = control::locate(&households, &group.coordinator)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let playlist =
                playlists::save_queue(&*self.transport, &households, &group.coordinator, title)?;
            Ok(SavedSonosPlaylistDto {
                done: format!(
                    "saved {}'s queue as Sonos playlist {:?} ({})",
                    player.room_name, playlist.title, playlist.id
                ),
                zone: player.room_name.clone(),
                playlist: SonosPlaylistDto::from(&playlist),
                logged: false,
            })
        })();
        match result {
            Ok(mut saved) => {
                saved.logged =
                    self.record(client, intent, "allow".into(), saved.done.clone(), None);
                Ok(saved)
            }
            Err(mut failure) => {
                if failure.code == ErrorCode::OutcomeUncertain {
                    failure = failure.with_hint(
                        "Read this household's saved Sonos playlists before saving again; the save was not automatically retried.",
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
    fn native_playlist_requests_require_one_bounded_name_and_no_renderer_metadata() {
        for playlist in ["SQ:1", "2", "Dvořák & friends"] {
            PlaySonosPlaylistRequest {
                zone: "Den@S1".into(),
                playlist: playlist.into(),
            }
            .validate()
            .unwrap();
        }
        for playlist in [
            String::new(),
            " \t ".into(),
            "A\nB".into(),
            "x".repeat(1025),
        ] {
            assert!(
                PlaySonosPlaylistRequest {
                    zone: "Den".into(),
                    playlist
                }
                .validate()
                .is_err()
            );
        }
        for body in [
            json!({"zone":"Den", "playlist":"SQ:1", "metadata":"untrusted"}),
            json!({"zone":"Den", "playlist":"SQ:1", "source_uri":"file:///untrusted"}),
        ] {
            assert!(serde_json::from_value::<PlaySonosPlaylistRequest>(body).is_err());
        }
    }

    #[test]
    fn saves_accept_only_a_room_and_a_bounded_utf8_title() {
        let request = |title: String| SaveSonosPlaylistRequest {
            zone: "Den@S1".into(),
            title,
        };
        for title in [
            "  Dvořák & friends  ".into(),
            "x".repeat(playlists::MAX_PLAYLIST_TITLE_BYTES),
            format!("{}x", "é".repeat(511)),
        ] {
            request(title).validate().unwrap();
        }
        for title in [
            String::new(),
            " \t ".into(),
            "A\nB".into(),
            "A\0B".into(),
            "x".repeat(playlists::MAX_PLAYLIST_TITLE_BYTES + 1),
            "é".repeat(512),
        ] {
            assert!(request(title).validate().is_err());
        }
        let mut bad_room = request("Mix".into());
        bad_room.zone = " ".into();
        assert!(bad_room.validate().is_err());
        for extra in [
            "playlist",
            "object_id",
            "overwrite",
            "source_uri",
            "metadata",
        ] {
            let mut body = json!({"zone":"Den", "title":"Evening"});
            body[extra] = json!("SQ:1");
            assert!(serde_json::from_value::<SaveSonosPlaylistRequest>(body).is_err());
        }
    }

    #[test]
    fn a_saved_catalog_item_remains_successful_when_local_action_logging_fails() {
        let saved = SavedSonosPlaylistDto {
            done: "saved Den's queue as Sonos playlist Evening (SQ:8)".into(),
            zone: "Den".into(),
            playlist: SonosPlaylistDto {
                id: "SQ:8".into(),
                title: "Evening".into(),
                art_uri: None,
            },
            logged: false,
        };
        assert!(saved.text().contains("playlist was saved"));
        assert!(saved.text().contains("do not repeat the save"));
        assert_eq!(
            SavedSonosPlaylistDto {
                logged: true,
                ..saved.clone()
            }
            .text(),
            format!("{}\n", saved.done)
        );
    }
}
