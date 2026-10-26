//! Read-only artist discovery when the app cannot use Spotify's top tracks.
//! The fallback is a catalog sample, not a substitute popularity ranking.

use super::{ARTIST_TRACKS, Artist, LibraryItem, Origin, Reader, endpoints_own, valid_taste_track};
use crate::SpotifyError;
use crate::client::{Album, ArtistTopTracks, PAGE_LIMIT, Paging, SimplifiedTrack, decode};
use crate::expand::halts;
use std::collections::HashSet;

/// One directory page, then at most three first track pages, per artist.
/// Never follow catalog pagination: this is a small taste sample. Whole
/// classical works are completed later through the normal album cache.
const CATALOG_ALBUM_LIMIT: u32 = 10;
const MAX_CATALOG_ALBUMS: usize = 3;

impl Reader<'_> {
    /// Up to three distinct playable tracks per artist. A 403/404 from the
    /// optional top-tracks endpoint selects the catalog path for the rest
    /// of this pass, avoiding a refused request for every followed artist.
    /// Failed artists remain unread so the sync retains their cached tracks.
    pub(super) async fn artist_tracks(
        &mut self,
        artists: &[&Artist],
    ) -> (Vec<LibraryItem>, Vec<String>) {
        let mut tracks = Vec::new();
        let mut unread = Vec::new();
        let mut catalog = false;
        for (index, artist) in artists.iter().enumerate() {
            let Some(id) = artist.id.as_deref() else {
                continue;
            };
            if self.read.halted.is_some() {
                unread.extend(
                    artists[index..]
                        .iter()
                        .map(|artist| artist.name.to_lowercase()),
                );
                break;
            }
            match self.artist_sample(id, &mut catalog).await {
                Ok(sample) => {
                    self.read.artist_tracks += sample.len();
                    tracks.extend(sample);
                }
                Err(error) if halts(&error) && !endpoints_own(&error) => {
                    self.read.halted = Some(format!("artist tracks: {error}"));
                    unread.extend(
                        artists[index..]
                            .iter()
                            .map(|artist| artist.name.to_lowercase()),
                    );
                    break;
                }
                Err(error) => {
                    self.read
                        .artist_tracks_unread
                        .get_or_insert_with(|| error.to_string());
                    if matches!(
                        error,
                        SpotifyError::Api { status: 404, .. } | SpotifyError::Decode(_)
                    ) {
                        // One unavailable or empty artist must not starve the
                        // later artists on every daily pass.
                        unread.push(artist.name.to_lowercase());
                        continue;
                    }
                    unread.extend(
                        artists[index..]
                            .iter()
                            .map(|artist| artist.name.to_lowercase()),
                    );
                    break;
                }
            }
        }
        (tracks, unread)
    }

    async fn artist_sample(
        &mut self,
        artist_id: &str,
        catalog: &mut bool,
    ) -> Result<Vec<LibraryItem>, SpotifyError> {
        if !*catalog {
            let url = self.session.endpoints().artist_top_tracks(artist_id);
            match self.session.get_brief(self.cx, &url).await {
                Ok(body) => {
                    let top: ArtistTopTracks = decode(&body, "artist top tracks")?;
                    let mut seen = HashSet::new();
                    return Ok(top
                        .tracks
                        .iter()
                        .filter_map(|track| track.library_item(Origin::Taste, None))
                        .filter(valid_taste_track)
                        .filter(|track| seen.insert(track.source_uri.clone()))
                        .take(ARTIST_TRACKS)
                        .collect());
                }
                Err(SpotifyError::Api {
                    status: 403 | 404, ..
                }) => *catalog = true,
                Err(error) => return Err(error),
            }
        }
        self.artist_catalog(artist_id).await
    }

    async fn artist_catalog(&mut self, artist_id: &str) -> Result<Vec<LibraryItem>, SpotifyError> {
        let url = self
            .session
            .endpoints()
            .artist_albums(artist_id, CATALOG_ALBUM_LIMIT);
        let body = self.session.get_brief(self.cx, &url).await?;
        let albums: Paging<Album> = decode(&body, "artist albums")?;
        if albums.offset != 0 || albums.items.len() > CATALOG_ALBUM_LIMIT as usize {
            return Err(SpotifyError::Decode(
                "invalid artist album sample page".into(),
            ));
        }
        let mut tracks = Vec::new();
        let mut seen_tracks = HashSet::new();
        let mut seen_albums = HashSet::new();
        let albums = albums
            .items
            .iter()
            .filter(|album| catalog_album(album, artist_id))
            .filter(|album| seen_albums.insert(album.id.as_str()))
            .take(MAX_CATALOG_ALBUMS);
        for album in albums {
            let url = self.session.endpoints().album_tracks(&album.id, 0);
            let body = self.session.get_brief(self.cx, &url).await?;
            let page: Paging<SimplifiedTrack> = decode(&body, "artist album tracks")?;
            if page.offset != 0 || page.items.len() > PAGE_LIMIT as usize {
                return Err(SpotifyError::Decode(
                    "invalid artist track sample page".into(),
                ));
            }
            for track in &page.items {
                // A shared release can contain another artist's solo tracks.
                // Only the followed/top artist's actual credits add taste.
                if !track
                    .artists
                    .iter()
                    .any(|artist| artist.id.as_deref() == Some(artist_id))
                {
                    continue;
                }
                if let Some(mut item) = album.library_item(track)
                    && valid_taste_track(&item)
                    && seen_tracks.insert(item.source_uri.clone())
                {
                    item.origin = Origin::Taste;
                    tracks.push(item);
                    if tracks.len() == ARTIST_TRACKS {
                        return Ok(tracks);
                    }
                }
            }
        }
        if tracks.is_empty() {
            // A bounded sample cannot prove the artist has no playable music.
            // Keep prior taste instead of retiring it on that assumption.
            return Err(SpotifyError::Decode(
                "no playable artist tracks in the bounded catalog sample".into(),
            ));
        }
        Ok(tracks)
    }
}

fn catalog_album(album: &Album, artist_id: &str) -> bool {
    !album.id.is_empty()
        && album.id.len() <= 128
        && album.id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        && album.uri == format!("spotify:album:{}", album.id)
        && album
            .album_type
            .as_deref()
            .is_none_or(|kind| matches!(kind, "album" | "single"))
        && album
            .artists
            .iter()
            .any(|artist| artist.id.as_deref() == Some(artist_id))
}
