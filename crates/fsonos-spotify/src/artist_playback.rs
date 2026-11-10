//! A bounded, read-only catalog selection for an explicitly requested artist.
//!
//! Sonos cannot start an artist from a Spotify artist id alone. Resolve that
//! request to concrete tracks before the control layer changes a speaker. This
//! is neither a complete discography nor a DJ pick: provider order and explicit
//! metadata are retained, without applying or changing the owner's taste.

use std::collections::HashSet;

use asupersync::Cx;

use crate::SpotifyError;
use crate::client::{
    Album, ArtistTopTracks, PAGE_LIMIT, Paging, SimplifiedArtist, SimplifiedTrack, decode,
};
use crate::library::{LibraryItem, Origin};
use crate::session::Session;

/// At most this many distinct playable tracks are returned.
pub const MAX_TRACKS: usize = 10;
/// The catalog fallback examines only the first ten albums and singles.
pub const CATALOG_ALBUM_LIMIT: u32 = 10;
/// At most this many distinct credited albums have their first track page read.
pub const MAX_CATALOG_ALBUMS: usize = 3;

/// Where the ordered selection came from. Neither variant means full coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionSource {
    /// Spotify's returned top-track order, limited to [`MAX_TRACKS`].
    TopTracks,
    /// Provider album/track order from the bounded catalog fallback, not a
    /// popularity ranking or a complete album/artist discography.
    CatalogSample,
}

/// A fully resolved selection, ready for the caller's normal playback policy.
/// Resolving reads metadata only; it does not publish library rows or play music.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtistPlayback {
    pub artist_uri: String,
    pub tracks: Vec<LibraryItem>,
    pub source: SelectionSource,
    /// Album-track pages read by the catalog fallback; zero for top tracks.
    pub albums_read: usize,
}

/// Resolve a canonical `spotify:artist:<id>` URI or a bare 22-character artist
/// id. The top-tracks endpoint is tried first; only a 403/404 selects the
/// bounded catalog fallback. Authentication, transport and cooldown failures
/// propagate, and an empty or malformed selection is an error before playback.
///
/// All URLs are built from the active session's endpoints. Returned `next`
/// links are never followed. Each logical read uses [`Session::get_brief`]'s
/// existing retry/deadline policy and server-requested cooldown.
pub async fn resolve(
    session: &mut Session,
    cx: &Cx,
    artist_uri: &str,
) -> Result<ArtistPlayback, SpotifyError> {
    let id = artist_uri
        .strip_prefix("spotify:artist:")
        .unwrap_or(artist_uri);
    if !valid_id(id) {
        return Err(SpotifyError::Config(
            "artist playback requires a Spotify artist id of 22 ASCII letters or digits".into(),
        ));
    }
    let url = session.endpoints().artist_top_tracks(id);
    match session.get_brief(cx, &url).await {
        Ok(body) => {
            let top: ArtistTopTracks = decode(&body, "artist top tracks")?;
            let mut tracks = Vec::new();
            let mut seen = HashSet::new();
            for track in top.tracks {
                if track.is_local || track.is_playable == Some(false) {
                    continue;
                }
                check_identity(track.id.as_deref(), &track.uri, "track")?;
                check_identity(
                    track.album.id.as_deref(),
                    track.album.uri.as_deref().unwrap_or(""),
                    "album",
                )?;
                if !credited(&track.artists, id) || !seen.insert(track.uri.clone()) {
                    continue;
                }
                if let Some(item) = track.library_item(Origin::Taste, None) {
                    tracks.push(item);
                }
                if tracks.len() == MAX_TRACKS {
                    break;
                }
            }
            selection(id, tracks, SelectionSource::TopTracks, 0)
        }
        Err(SpotifyError::Api {
            status: 403 | 404, ..
        }) => catalog(session, cx, id).await,
        Err(error) => Err(error),
    }
}

fn valid_id(id: &str) -> bool {
    id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn check_identity(id: Option<&str>, uri: &str, kind: &str) -> Result<(), SpotifyError> {
    if id.is_some_and(|id| valid_id(id) && uri == format!("spotify:{kind}:{id}")) {
        Ok(())
    } else {
        Err(SpotifyError::Decode(format!(
            "artist playback received a malformed or inconsistent Spotify {kind} identity"
        )))
    }
}

fn credited(artists: &[SimplifiedArtist], artist_id: &str) -> bool {
    artists
        .iter()
        .any(|artist| artist.id.as_deref() == Some(artist_id))
}

fn check_page<T>(page: &Paging<T>, limit: u32) -> Result<(), SpotifyError> {
    if page.offset != 0
        || page.items.len() > limit as usize
        || page.items.len() > page.total as usize
    {
        return Err(SpotifyError::Decode(
            "artist playback received inconsistent catalog page counters".into(),
        ));
    }
    Ok(())
}

fn selection(
    id: &str,
    tracks: Vec<LibraryItem>,
    source: SelectionSource,
    albums_read: usize,
) -> Result<ArtistPlayback, SpotifyError> {
    if tracks.is_empty() {
        return Err(SpotifyError::Decode(
            "no playable tracks for this artist in the bounded Spotify selection".into(),
        ));
    }
    Ok(ArtistPlayback {
        artist_uri: format!("spotify:artist:{id}"),
        tracks,
        source,
        albums_read,
    })
}

async fn catalog(
    session: &mut Session,
    cx: &Cx,
    artist_id: &str,
) -> Result<ArtistPlayback, SpotifyError> {
    let url = session
        .endpoints()
        .artist_albums(artist_id, CATALOG_ALBUM_LIMIT);
    let body = session.get_brief(cx, &url).await?;
    let albums: Paging<Album> = decode(&body, "artist albums")?;
    check_page(&albums, CATALOG_ALBUM_LIMIT)?;
    let mut tracks = Vec::new();
    let mut seen_tracks = HashSet::new();
    let mut seen_albums = HashSet::new();
    let mut albums_read = 0;
    for album in albums.items {
        if !credited(&album.artists, artist_id)
            || album
                .album_type
                .as_deref()
                .is_some_and(|kind| !matches!(kind, "album" | "single"))
        {
            continue;
        }
        check_identity(Some(&album.id), &album.uri, "album")?;
        if !seen_albums.insert(album.id.clone()) {
            continue;
        }
        let url = session.endpoints().album_tracks(&album.id, 0);
        let body = session.get_brief(cx, &url).await?;
        let page: Paging<SimplifiedTrack> = decode(&body, "artist album tracks")?;
        check_page(&page, PAGE_LIMIT)?;
        albums_read += 1;
        for track in page.items {
            if track.is_local || track.is_playable == Some(false) {
                continue;
            }
            check_identity(track.id.as_deref(), &track.uri, "track")?;
            if !credited(&track.artists, artist_id) || !seen_tracks.insert(track.uri.clone()) {
                continue;
            }
            if let Some(mut item) = album.library_item(&track) {
                item.origin = Origin::Taste;
                tracks.push(item);
            }
            if tracks.len() == MAX_TRACKS {
                return selection(
                    artist_id,
                    tracks,
                    SelectionSource::CatalogSample,
                    albums_read,
                );
            }
        }
        if albums_read == MAX_CATALOG_ALBUMS {
            break;
        }
    }
    selection(
        artist_id,
        tracks,
        SelectionSource::CatalogSample,
        albums_read,
    )
}

#[cfg(test)]
mod tests;
