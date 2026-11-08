//! Saved Sonos queues (`SQ:`), independent of My Sonos favorites and the
//! Spotify library. Resolve names only against the target household's current
//! list, then use the existing native-container playback and policy boundary.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::favorites::{self, Favorite, FavoriteError};
use fsonos_core::playlists;
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, resolve, room_view};
use crate::OutcomeDto;
use crate::failure::{ErrorCode, Failure};
use crate::plan::Command;
use crate::request::zone_name;

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
}
