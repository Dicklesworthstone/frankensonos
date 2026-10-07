//! Library items: the neutral metadata the DJ's candidate pool is built from.
//!
//! The Spotify library reads ([`crate::client`]) and the local library cache
//! both produce [`LibraryItem`]s, and [`crate::classical::CandidatePool`]
//! consumes them. Keeping this shape independent of the Web API JSON lets the
//! daemon rebuild the pool from the store at startup without a network call.

use fsonos_types::Track;
use serde::{Deserialize, Serialize};

use crate::classical::normalize;

/// How a track entered the owner's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Origin {
    /// A track on one of the owner's saved albums.
    SavedAlbum,
    /// An individually liked ("saved") track.
    LikedTrack,
    /// Both liked and on a saved album.
    Both,
}

impl Origin {
    /// Combine the origins of two sightings of the same track.
    #[must_use]
    pub fn merge(self, other: Self) -> Self {
        if self == other { self } else { Self::Both }
    }

    /// The owner explicitly liked this track (the DJ favors these slightly).
    #[must_use]
    pub fn is_liked(self) -> bool {
        matches!(self, Self::LikedTrack | Self::Both)
    }
}

/// One playable track from the owner's library, with the metadata the
/// classical heuristics read. `source_uri` is the `spotify:track:…` URI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub source_uri: String,
    pub title: String,
    /// Track artists in credit order. Spotify credits the composer here for
    /// classical recordings, usually first.
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_uri: Option<String>,
    pub album_artists: Vec<String>,
    /// Album/artist genres when the read returned any (often empty).
    pub genres: Vec<String>,
    /// Record label when available (Spotify dropped it for new apps in 2026).
    pub label: Option<String>,
    pub duration_secs: Option<u32>,
    pub explicit: bool,
    pub origin: Origin,
}

impl LibraryItem {
    /// Rebuild an item from a cached [`Track`]. The cache stores artists as
    /// one string joined by [`ARTIST_SEPARATOR`] (see [`Self::to_track`]).
    #[must_use]
    pub fn from_track(track: &Track, origin: Origin) -> Self {
        let artists = track
            .artist
            .as_deref()
            .map(|a| {
                a.split(ARTIST_SEPARATOR)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        Self {
            source_uri: track.source_uri.clone(),
            title: track.title.clone(),
            artists,
            album: track.album.clone(),
            album_uri: None,
            album_artists: Vec::new(),
            genres: Vec::new(),
            label: None,
            duration_secs: track.duration_secs,
            explicit: false,
            origin,
        }
    }

    /// The source-agnostic [`Track`] for this item (renderer `uri` unset —
    /// Lane A/B resolve it per household).
    #[must_use]
    pub fn to_track(&self) -> Track {
        Track {
            title: self.title.clone(),
            artist: (!self.artists.is_empty()).then(|| self.artists.join(ARTIST_SEPARATOR)),
            album: self.album.clone(),
            source_uri: self.source_uri.clone(),
            uri: None,
            duration_secs: self.duration_secs,
        }
    }

    /// A stable key grouping tracks of the same album: the album URI when
    /// known, else the normalized album name and album artists.
    #[must_use]
    pub fn album_key(&self) -> Option<String> {
        if let Some(uri) = &self.album_uri {
            return Some(uri.clone());
        }
        let album = normalize(self.album.as_deref()?);
        if album.is_empty() {
            return None;
        }
        Some(format!(
            "{album}|{}",
            normalize(&self.album_artists.join(" "))
        ))
    }

    /// Fold a second sighting of the same track into this one: merge the
    /// origin and fill any metadata this sighting lacked.
    pub fn absorb(&mut self, other: &Self) {
        self.origin = self.origin.merge(other.origin);
        if self.album.is_none() {
            self.album.clone_from(&other.album);
        }
        if self.album_uri.is_none() {
            self.album_uri.clone_from(&other.album_uri);
        }
        if self.album_artists.is_empty() {
            self.album_artists.clone_from(&other.album_artists);
        }
        if self.genres.is_empty() {
            self.genres.clone_from(&other.genres);
        }
        if self.label.is_none() {
            self.label.clone_from(&other.label);
        }
        if self.duration_secs.is_none() {
            self.duration_secs = other.duration_secs;
        }
        self.explicit |= other.explicit;
    }
}

/// Joins multiple artists into [`Track::artist`] for the library cache.
pub const ARTIST_SEPARATOR: &str = "; ";

#[cfg(test)]
mod tests {
    use super::*;

    fn item(uri: &str, origin: Origin) -> LibraryItem {
        LibraryItem {
            source_uri: uri.into(),
            title: "Cello Suite No. 1 in G Major, BWV 1007: I. Prélude".into(),
            artists: vec!["Johann Sebastian Bach".into(), "Yo-Yo Ma".into()],
            album: None,
            album_uri: None,
            album_artists: Vec::new(),
            genres: Vec::new(),
            label: None,
            duration_secs: Some(150),
            explicit: false,
            origin,
        }
    }

    #[test]
    fn origin_merge() {
        assert_eq!(
            Origin::SavedAlbum.merge(Origin::SavedAlbum),
            Origin::SavedAlbum
        );
        assert_eq!(Origin::SavedAlbum.merge(Origin::LikedTrack), Origin::Both);
        assert_eq!(Origin::Both.merge(Origin::LikedTrack), Origin::Both);
        assert!(Origin::Both.is_liked() && Origin::LikedTrack.is_liked());
        assert!(!Origin::SavedAlbum.is_liked());
    }

    #[test]
    fn track_round_trip_keeps_artists() {
        let it = item("spotify:track:a", Origin::LikedTrack);
        let track = it.to_track();
        assert_eq!(
            track.artist.as_deref(),
            Some("Johann Sebastian Bach; Yo-Yo Ma")
        );
        let back = LibraryItem::from_track(&track, Origin::LikedTrack);
        assert_eq!(back.artists, it.artists);
        assert_eq!(back.title, it.title);
        assert_eq!(back.duration_secs, Some(150));
    }

    #[test]
    fn absorb_merges_origin_and_fills_gaps() {
        let mut liked = item("spotify:track:a", Origin::LikedTrack);
        let mut on_album = item("spotify:track:a", Origin::SavedAlbum);
        on_album.album = Some("Bach: Cello Suites".into());
        on_album.album_uri = Some("spotify:album:x".into());
        liked.absorb(&on_album);
        assert_eq!(liked.origin, Origin::Both);
        assert_eq!(liked.album_uri.as_deref(), Some("spotify:album:x"));
        assert_eq!(liked.album_key().as_deref(), Some("spotify:album:x"));
    }

    #[test]
    fn album_key_falls_back_to_names() {
        let mut it = item("spotify:track:a", Origin::LikedTrack);
        assert_eq!(it.album_key(), None);
        it.album = Some("Bach: Cello Suites".into());
        it.album_artists = vec!["Yo-Yo Ma".into()];
        assert_eq!(
            it.album_key().as_deref(),
            Some("bach cello suites|yo yo ma")
        );
    }
}
