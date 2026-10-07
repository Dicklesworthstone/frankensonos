//! What the read tools and routes answer: a zone's live state and a
//! household's favorites.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::favorites::{Favorite, FavoriteKind};
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
