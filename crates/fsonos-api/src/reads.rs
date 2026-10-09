//! What the read tools and routes answer: a zone's live state and a
//! household's favorites.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::favorites::{Favorite, FavoriteKind};
use fsonos_core::search::{Hit, HitSource};
use fsonos_proto::control::PositionInfo;
use serde::{Deserialize, Serialize};

use crate::zones::ZoneDto;

/// `GET /zones/{room}/state` and the `get_zone_state` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ZoneStateDto {
    /// The group the room plays in.
    pub zone: ZoneDto,
    /// `playing`, `paused`, `stopped`, `transitioning` or `unknown`.
    pub transport_state: String,
    /// The room's own volume (0–100), when its player answers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// What the group is on, when anything is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track: Option<TrackDto>,
}

/// The current track (or stream) of a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct TrackDto {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// The artist or composer, as the speaker reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    pub uri: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_secs: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_secs: Option<u32>,
    /// 1-based position in the queue; absent when not playing from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_position: Option<u32>,
}

impl TrackDto {
    /// The track in `position`, or `None` when the group is on nothing.
    #[must_use]
    pub fn from_position(position: &PositionInfo) -> Option<Self> {
        if position.uri.is_empty() {
            return None;
        }
        let meta = position.metadata.as_ref();
        Some(Self {
            title: meta.map(|m| m.title.clone()).filter(|t| !t.is_empty()),
            creator: meta.and_then(|m| m.creator.clone()),
            album: meta.and_then(|m| m.album.clone()),
            uri: position.uri.clone(),
            duration_secs: position.duration_secs,
            position_secs: position.position_secs,
            queue_position: (position.track > 0).then_some(position.track),
        })
    }
}

/// One Sonos favorite: `GET /favorites` and the `list_favorites` tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct FavoriteDto {
    /// `FV:2/<n>`; also accepted by `play_favorite`.
    pub id: String,
    pub title: String,
    /// `track`, `stream`, `container` (replaces the queue) or `unplayable`.
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_uri: Option<String>,
}

impl From<&Favorite> for FavoriteDto {
    fn from(f: &Favorite) -> Self {
        let kind = match f.kind {
            FavoriteKind::Track => "track",
            FavoriteKind::Stream => "stream",
            FavoriteKind::Container => "container",
            FavoriteKind::Unplayable => "unplayable",
        };
        Self {
            id: f.id.clone(),
            title: f.title.clone(),
            kind: kind.to_string(),
            description: f.description.clone(),
            art_uri: f.art_uri.clone(),
        }
    }
}

/// One room: `GET /rooms`, the `list_rooms` tool and `fsonos rooms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomDto {
    pub name: String,
    /// The household (`S1`, `S2`, or its id); `Name@<household>` names the
    /// room unambiguously.
    pub household: String,
    /// The room whose player coordinates the group this room plays in.
    pub zone: String,
    /// The owner's aliases (`aliases.toml`) that name this room, alone or
    /// with others.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

/// One library search result: `search_library` and `GET /library/search`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct HitDto {
    /// `track`, `album` or `playlist` (play it with `play` and
    /// `source_uri`; an album or playlist replaces the group's queue) or
    /// `favorite` (play it with `play_favorite` and `favorite`, in the same
    /// household).
    pub kind: String,
    pub title: String,
    /// The artists of a track or album, or a favorite's description.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    /// `spotify:track:<id>`, `spotify:album:<id>` or `spotify:playlist:<id>`,
    /// for `play`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// `FV:2/<n>`, for `play_favorite`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
    /// Higher is a better match.
    pub score: u32,
}

impl From<&Hit> for HitDto {
    fn from(hit: &Hit) -> Self {
        let (kind, source_uri, favorite) = match &hit.source {
            HitSource::Library { source_uri } => ("track", Some(source_uri.clone()), None),
            HitSource::Favorite { id } => ("favorite", None, Some(id.clone())),
            HitSource::Album { source_uri } => ("album", Some(source_uri.clone()), None),
            HitSource::Playlist { source_uri } => ("playlist", Some(source_uri.clone()), None),
        };
        Self {
            kind: kind.to_string(),
            title: hit.title.clone(),
            subtitle: hit.subtitle.clone(),
            source_uri,
            favorite,
            score: hit.score,
        }
    }
}

/// One recorded play: `recent_plays` and `GET /history`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlayDto {
    /// The room whose group played it (its coordinator's room), when the
    /// households still have that player.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub room: Option<String>,
    pub source_uri: String,
    /// Unix seconds.
    pub played_at: i64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_transport_has_no_track() {
        let idle = PositionInfo {
            track: 0,
            duration_secs: None,
            position_secs: None,
            uri: String::new(),
            metadata: None,
        };
        assert_eq!(TrackDto::from_position(&idle), None);
        let stream = PositionInfo {
            uri: "x-rincon-mp3radio://stream.example.org/a.mp3".into(),
            ..idle
        };
        let track = TrackDto::from_position(&stream).unwrap();
        assert_eq!((track.queue_position, track.title.as_deref()), (None, None));
        let json = serde_json::to_value(&track).unwrap();
        assert_eq!(
            json,
            serde_json::json!({ "uri": "x-rincon-mp3radio://stream.example.org/a.mp3" })
        );
    }

    #[test]
    fn favorites_name_their_kind() {
        let fav = Favorite {
            id: "FV:2/3".into(),
            title: "Sim Symphonies".into(),
            kind: FavoriteKind::Container,
            uri: Some("x-rincon-cpcontainer:abc".into()),
            metadata: String::new(),
            description: Some("Spotify".into()),
            art_uri: None,
        };
        let dto = FavoriteDto::from(&fav);
        assert_eq!(
            (dto.kind.as_str(), dto.id.as_str()),
            ("container", "FV:2/3")
        );
    }
}
