//! Library items: the neutral metadata the DJ's candidate pool is built from.
//!
//! The Spotify library reads ([`LibraryRead`] over [`crate::client`]) and the
//! local library cache both produce [`LibraryItem`]s, and
//! [`crate::classical::CandidatePool`] consumes them. Keeping this shape
//! independent of the Web API JSON lets the daemon rebuild the pool from the
//! store at startup without a network call.

use std::collections::{HashMap, HashSet, VecDeque};

use fsonos_types::Track;
use serde::{Deserialize, Serialize};

use crate::SpotifyError;
use crate::classical::normalize;
use crate::client::{Album, Endpoints, Paging, SavedAlbum, SavedTrack, SimplifiedTrack};

/// How a track entered the owner's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Origin {
    /// A track on one of the owner's saved albums.
    #[default]
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

/// One playable track from the owner's library, with the metadata the DJ
/// reads. `source_uri` is the `spotify:track:…` URI. Tests build one with
/// `..LibraryItem::default()`, so a new field doesn't break them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LibraryItem {
    pub source_uri: String,
    pub title: String,
    /// Track artists in credit order. Spotify credits the composer here for
    /// classical recordings, usually first.
    pub artists: Vec<String>,
    pub album: Option<String>,
    pub album_uri: Option<String>,
    pub album_artists: Vec<String>,
    /// Position on the album, when known (orders a work's movements).
    #[serde(default)]
    pub disc_number: Option<u32>,
    #[serde(default)]
    pub track_number: Option<u32>,
    /// When the owner saved it (the album, for album tracks), Unix seconds.
    #[serde(default)]
    pub added_at: Option<i64>,
    /// Album/artist genres when the read returned any (often empty).
    pub genres: Vec<String>,
    /// The album's release year, when the read gave a release date.
    #[serde(default)]
    pub release_year: Option<u16>,
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
        Self {
            source_uri: track.source_uri.clone(),
            title: track.title.clone(),
            artists: track
                .artist
                .as_deref()
                .map(split_artists)
                .unwrap_or_default(),
            album: track.album.clone(),
            album_uri: None,
            album_artists: Vec::new(),
            disc_number: None,
            track_number: None,
            added_at: None,
            genres: Vec::new(),
            release_year: None,
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
        if self.release_year.is_none() {
            self.release_year = other.release_year;
        }
        if self.label.is_none() {
            self.label.clone_from(&other.label);
        }
        if self.duration_secs.is_none() {
            self.duration_secs = other.duration_secs;
        }
        if self.track_number.is_none() {
            self.disc_number = other.disc_number;
            self.track_number = other.track_number;
        }
        self.added_at = match (self.added_at, other.added_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.explicit |= other.explicit;
    }
}

/// Split an [`ARTIST_SEPARATOR`]-joined artist string back into names.
#[must_use]
pub fn split_artists(joined: &str) -> Vec<String> {
    joined
        .split(ARTIST_SEPARATOR)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

/// One item per `source_uri`, in first-sighting order, with later sightings
/// (a liked track that is also on a saved album) folded in by
/// [`LibraryItem::absorb`].
#[must_use]
pub fn merge_duplicates(items: &[LibraryItem]) -> Vec<LibraryItem> {
    let mut merged: Vec<LibraryItem> = Vec::new();
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for item in items {
        if let Some(&i) = seen.get(item.source_uri.as_str()) {
            merged[i].absorb(item);
        } else {
            seen.insert(&item.source_uri, merged.len());
            merged.push(item.clone());
        }
    }
    merged
}

/// Joins multiple artists into [`Track::artist`] for the library cache.
pub const ARTIST_SEPARATOR: &str = "; ";

/// A full read of the owner's library as a sans-I/O state machine: GET
/// [`Self::next_url`] with the bearer token, feed each 2xx body to
/// [`Self::ingest`], repeat until `next_url` is `None`, then take
/// [`Self::into_items`]. It pages saved albums and liked tracks, fetches the
/// rest of any album longer than its embedded 50 tracks, and only follows
/// links that point at the Web API (and never the same one twice).
#[derive(Debug)]
pub struct LibraryRead {
    endpoints: Endpoints,
    pending: VecDeque<Fetch>,
    requested: HashSet<String>,
    items: Vec<LibraryItem>,
    pages: usize,
}

#[derive(Debug)]
enum Fetch {
    SavedAlbums(String),
    SavedTracks(String),
    /// Later pages of one album's tracks, with the album (and when it was
    /// saved) as context.
    AlbumTracks(String, Box<Album>, Option<i64>),
}

impl Fetch {
    fn url(&self) -> &str {
        match self {
            Self::SavedAlbums(url) | Self::SavedTracks(url) | Self::AlbumTracks(url, ..) => url,
        }
    }
}

impl Default for LibraryRead {
    /// A read against Spotify's own Web API.
    fn default() -> Self {
        Self::new(&Endpoints::default())
    }
}

impl LibraryRead {
    #[must_use]
    pub fn new(endpoints: &Endpoints) -> Self {
        let mut read = Self {
            endpoints: endpoints.clone(),
            pending: VecDeque::new(),
            requested: HashSet::new(),
            items: Vec::new(),
            pages: 0,
        };
        read.queue(Fetch::SavedAlbums(endpoints.saved_albums(0)));
        read.queue(Fetch::SavedTracks(endpoints.saved_tracks(0)));
        read
    }

    /// The next URL to GET, or `None` when the read is complete.
    #[must_use]
    pub fn next_url(&self) -> Option<&str> {
        self.pending.front().map(Fetch::url)
    }

    /// Feed the body of a successful response to [`Self::next_url`].
    pub fn ingest(&mut self, body: &[u8]) -> Result<(), SpotifyError> {
        let fetch = self
            .pending
            .pop_front()
            .ok_or_else(|| SpotifyError::Decode("library read already complete".into()))?;
        self.pages += 1;
        match fetch {
            Fetch::SavedAlbums(_) => {
                let page = Paging::<SavedAlbum>::parse(body)?;
                for saved in page.items {
                    self.items.extend(saved.library_items());
                    let added_at = saved.added_unix();
                    let rest = saved.album.tracks.as_ref().and_then(|t| t.next.clone());
                    if let Some(url) = rest {
                        let album = Box::new(Album {
                            tracks: None,
                            ..saved.album
                        });
                        self.follow(Some(url), |url| Fetch::AlbumTracks(url, album, added_at))?;
                    }
                }
                self.follow(page.next, Fetch::SavedAlbums)
            }
            Fetch::SavedTracks(_) => {
                let page = Paging::<SavedTrack>::parse(body)?;
                self.items
                    .extend(page.items.iter().filter_map(SavedTrack::library_item));
                self.follow(page.next, Fetch::SavedTracks)
            }
            Fetch::AlbumTracks(_, album, added_at) => {
                let page = Paging::<SimplifiedTrack>::parse(body)?;
                self.items.extend(page.items.iter().filter_map(|t| {
                    let mut item = album.library_item(t)?;
                    item.added_at = added_at;
                    Some(item)
                }));
                self.follow(page.next, |url| Fetch::AlbumTracks(url, album, added_at))
            }
        }
    }

    /// Pages ingested so far.
    #[must_use]
    pub fn pages_read(&self) -> usize {
        self.pages
    }

    /// Everything read, in read order (duplicates are merged later by
    /// [`crate::classical::CandidatePool::build`]).
    #[must_use]
    pub fn into_items(self) -> Vec<LibraryItem> {
        self.items
    }

    fn follow(
        &mut self,
        next: Option<String>,
        fetch: impl FnOnce(String) -> Fetch,
    ) -> Result<(), SpotifyError> {
        let Some(url) = next else {
            return Ok(());
        };
        if !self.endpoints.is_api_url(&url) {
            return Err(SpotifyError::Decode(format!(
                "refusing to follow a paging link off the Web API: {url}"
            )));
        }
        if self.requested.contains(&url) {
            return Err(SpotifyError::Decode(format!("paging loop at {url}")));
        }
        self.queue(fetch(url));
        Ok(())
    }

    fn queue(&mut self, fetch: Fetch) {
        self.requested.insert(fetch.url().to_owned());
        self.pending.push_back(fetch);
    }
}

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
            disc_number: None,
            track_number: None,
            added_at: None,
            genres: Vec::new(),
            release_year: None,
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
        on_album.disc_number = Some(2);
        on_album.track_number = Some(4);
        liked.absorb(&on_album);
        assert_eq!(liked.origin, Origin::Both);
        assert_eq!((liked.disc_number, liked.track_number), (Some(2), Some(4)));
        assert_eq!(liked.album_uri.as_deref(), Some("spotify:album:x"));
        assert_eq!(liked.album_key().as_deref(), Some("spotify:album:x"));
    }

    const ALBUMS_PAGE_2: &str =
        "https://api.spotify.com/v1/me/albums?offset=50&limit=50&market=from_token";
    const CHOPIN_REST: &str =
        "https://api.spotify.com/v1/albums/FakeAlbum0000000000002/tracks?offset=50&limit=50";

    fn chopin_rest_page(next: Option<&str>) -> String {
        serde_json::json!({
            "items": [{
                "artists": [{ "name": "Frédéric Chopin" }],
                "duration_ms": 330_000,
                "id": "FakeTrack0000000000008",
                "is_playable": true,
                "name": "Nocturnes, Op. 48: No. 1 in C Minor",
                "uri": "spotify:track:FakeTrack0000000000008"
            }],
            "next": next, "offset": 50, "limit": 50, "total": 60
        })
        .to_string()
    }

    #[test]
    fn library_read_pages_everything() {
        let albums_last = r#"{"items": [], "next": null, "offset": 50, "limit": 50, "total": 51}"#;
        let chopin_rest = chopin_rest_page(None);
        let body = |url: &str| -> &[u8] {
            match url {
                u if u == Endpoints::default().saved_albums(0) => {
                    include_bytes!("../tests/fixtures/saved_albums_page.json")
                }
                u if u == Endpoints::default().saved_tracks(0) => {
                    include_bytes!("../tests/fixtures/saved_tracks_page.json")
                }
                ALBUMS_PAGE_2 => albums_last.as_bytes(),
                CHOPIN_REST => chopin_rest.as_bytes(),
                other => panic!("unexpected GET {other}"),
            }
        };

        let mut read = LibraryRead::default();
        let mut fetched = Vec::new();
        while let Some(url) = read.next_url().map(str::to_owned) {
            read.ingest(body(&url)).unwrap();
            fetched.push(url);
        }
        assert_eq!(fetched.len(), 4);
        assert_eq!(read.pages_read(), 4);
        assert!(read.ingest(b"{}").is_err(), "nothing outstanding");

        let items = read.into_items();
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Goldberg Variations, BWV 988: Aria",
                "Goldberg Variations, BWV 988: Variatio 1 a 1 Clav.",
                "Goldberg Variations, BWV 988: Variatio 2 a 1 Clav.",
                "Nocturnes, Op. 9: No. 2 in E-Flat Major",
                "Suite bergamasque, L. 75: III. Clair de lune",
                "Nocturnes, Op. 48: No. 1 in C Minor",
            ]
        );
        // Later album pages keep the album's context.
        let late = items.last().unwrap();
        assert_eq!(late.album.as_deref(), Some("Chopin: Complete Nocturnes"));
        assert_eq!(
            late.album_uri.as_deref(),
            Some("spotify:album:FakeAlbum0000000000002")
        );
        assert_eq!(late.origin, Origin::SavedAlbum);

        // And the whole read feeds the DJ's pool.
        let pool = crate::classical::CandidatePool::build(&items);
        assert_eq!(pool.len(), 6);
    }

    #[test]
    fn library_read_refuses_foreign_and_looping_links() {
        let mut read = LibraryRead::default();
        let foreign = r#"{"items": [], "next": "https://evil.example/v1/me/albums?offset=50"}"#;
        let err = read.ingest(foreign.as_bytes()).unwrap_err().to_string();
        assert!(err.contains("off the Web API"), "{err}");

        let mut read = LibraryRead::default();
        read.ingest(include_bytes!("../tests/fixtures/saved_albums_page.json"))
            .unwrap();
        read.ingest(include_bytes!("../tests/fixtures/saved_tracks_page.json"))
            .unwrap();
        assert_eq!(read.next_url(), Some(CHOPIN_REST));
        read.ingest(chopin_rest_page(Some(CHOPIN_REST)).as_bytes())
            .unwrap_err();
    }

    #[test]
    fn positions_survive_the_cache_round_trip() {
        let mut it = item("spotify:track:a", Origin::Both);
        it.album_uri = Some("spotify:album:x".into());
        it.disc_number = Some(2);
        it.track_number = Some(11);
        let json = serde_json::to_string(&it).unwrap();
        assert_eq!(serde_json::from_str::<LibraryItem>(&json).unwrap(), it);
        // Rows cached before positions existed still load, as unknown.
        let mut old: serde_json::Value = serde_json::from_str(&json).unwrap();
        old.as_object_mut().unwrap().remove("disc_number");
        old.as_object_mut().unwrap().remove("track_number");
        let loaded: LibraryItem = serde_json::from_value(old).unwrap();
        assert_eq!((loaded.disc_number, loaded.track_number), (None, None));
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
