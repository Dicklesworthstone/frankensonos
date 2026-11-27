//! Durable native selections keep the speaker's exact ordered occurrences.
//! The playlist is provenance; later catalog edits do not rewrite a saved scene.

use super::{SceneHooks, SonosPlaylistReference, sources};
use crate::favorites::{Favorite, FavoriteKind};
use crate::{CoreError, HouseholdState, control, playlists};
use fsonos_proto::content::MAX_BROWSE_OBJECTS;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

/// One selected native occurrence, including the complete speaker-owned DIDL.
/// Surfaces expose its resource and title, never accept authored renderer metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SceneNativeTrack {
    pub id: String,
    pub title: String,
    pub uri: String,
    pub metadata: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art_uri: Option<String>,
}

impl SceneNativeTrack {
    fn from_favorite(track: Favorite) -> Result<Self, CoreError> {
        playlists::validate_native_track(&track)?;
        Ok(Self {
            id: track.id,
            title: track.title,
            uri: track.uri.expect("validated native tracks have a resource"),
            metadata: track.metadata,
            description: track.description,
            art_uri: track.art_uri,
        })
    }

    fn favorite(&self) -> Favorite {
        Favorite {
            id: self.id.clone(),
            title: self.title.clone(),
            kind: FavoriteKind::Track,
            uri: Some(self.uri.clone()),
            metadata: self.metadata.clone(),
            description: self.description.clone(),
            art_uri: self.art_uri.clone(),
        }
    }
}

/// A frozen selection from one native saved playlist. The household remains
/// authoritative on recall; the original playlist can be edited or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTrackSelection {
    pub playlist: SonosPlaylistReference,
    /// One-based positions from the observed playlist, in the requested order.
    pub positions: Vec<u32>,
    pub tracks: Vec<SceneNativeTrack>,
}

fn invalid(detail: impl std::fmt::Display) -> CoreError {
    ProtoError::Malformed(format!("native track scene refused: {detail}")).into()
}

impl NativeTrackSelection {
    pub(super) fn validate(&self) -> Result<(), String> {
        self.playlist.validate()?;
        if self.tracks.is_empty()
            || self.tracks.len() > control::MAX_TRACK_BATCH
            || self.positions.len() != self.tracks.len()
            || self
                .positions
                .iter()
                .any(|position| *position == 0 || *position > MAX_BROWSE_OBJECTS)
        {
            return Err(
                "a native track scene needs 1–10 selected occurrences and their positions".into(),
            );
        }
        let mut bytes = 0usize;
        for track in &self.tracks {
            if track.uri.len() > 65_536
                || track.title.chars().count() > 4_096
                || track
                    .title
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r'))
            {
                return Err(
                    "native track scene identity or title exceeds its display limits".into(),
                );
            }
            bytes = bytes
                .checked_add(track.metadata.len())
                .ok_or_else(|| "native track scene metadata is too large".to_string())?;
            if bytes > super::MAX_QUEUE_BACKUP_BYTES {
                return Err("native track scene metadata exceeds 4 MiB".into());
            }
            playlists::validate_native_track(&track.favorite())
                .map_err(|error| error.to_string())?;
        }
        Ok(())
    }

    /// Resolve a reviewed playlist selection without changing any speaker.
    /// The same revision, source player and household must authorize every
    /// selected occurrence; repeats remain separate entries in the saved scene.
    pub fn capture<T: Transport + ?Sized>(
        t: &T,
        households: &[HouseholdState],
        source: &PlayerId,
        playlist_id: &str,
        positions: &[u32],
        expected_revision: &str,
    ) -> Result<Self, CoreError> {
        let playlist = SonosPlaylistReference::capture(t, households, source, playlist_id)?;
        let tracks = playlists::select_tracks(
            t,
            households,
            source,
            playlist_id,
            positions,
            expected_revision,
        )?
        .into_iter()
        .map(SceneNativeTrack::from_favorite)
        .collect::<Result<Vec<_>, _>>()?;
        let selected = Self {
            playlist,
            positions: positions.to_vec(),
            tracks,
        };
        selected.validate().map_err(invalid)?;
        Ok(selected)
    }

    pub(super) fn check_household(
        &self,
        households: &[HouseholdState],
        player: &PlayerId,
    ) -> Result<(), CoreError> {
        self.validate().map_err(invalid)?;
        self.playlist.check_household(households, player)
    }
}

pub(super) fn recall<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    selection: &NativeTrackSelection,
    start: bool,
    hooks: &dyn SceneHooks,
) -> Result<(), String> {
    selection
        .check_household(households, coordinator)
        .map_err(|error| error.to_string())?;
    sources::confirm_audience(t, households, coordinator).map_err(|error| error.to_string())?;
    let host = control::locate(households, coordinator)
        .map_err(|error| error.to_string())?
        .ip;
    let destination = control::TrackBatchDestination::read(t, host, coordinator)
        .map_err(|error| error.to_string())?;
    sources::confirm_audience(t, households, coordinator).map_err(|error| error.to_string())?;
    let tracks: Vec<_> = selection
        .tracks
        .iter()
        .map(SceneNativeTrack::favorite)
        .collect();
    let record = hooks.queue_replacement_started(coordinator);
    let receipt = if start {
        control::play_native_tracks_bound_with_receipt(
            t,
            households,
            coordinator,
            &tracks,
            &destination,
        )
    } else {
        control::load_native_tracks_bound_with_receipt(
            t,
            households,
            coordinator,
            &tracks,
            &destination,
        )
    }
    .map_err(|error| error.to_string())?;
    if record {
        hooks.queue_replacement_finished(coordinator, receipt);
    }
    Ok(())
}
