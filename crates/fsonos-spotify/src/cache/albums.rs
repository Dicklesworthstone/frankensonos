//! Finish whole-work album metadata independently of the daily library read.
//! An unfinished album stays in the private scheduler checkpoint. Only a
//! complete, validated list enters the album cache the selector consumes.

use super::playlists::{candidate_items, summarize};
use super::{LibrarySyncDetailed, pool_from_store};
use crate::SpotifyError;
use crate::client::{Endpoints, SimplifiedTrack};
use crate::expand::{
    AlbumMisses, MAX_ALBUM_ENTRIES, MAX_ALBUM_PAGES, album_id, album_track, halts,
};
use crate::genres::GenreRead;
use crate::library::Page;
use crate::session::Session;
use crate::taste::PlaylistContinuation;
use crate::works::group_works;
use asupersync::Cx;
use fsonos_core::store::{AlbumTrack, SpotifyLibraryOwner, Store};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Catalog pages per background pass, plus one account identity read.
pub const ALBUM_PAGE_BUDGET: usize = 8;
/// A private partial list cannot grow beyond this many metadata bytes.
const MAX_PARTIAL_BYTES: usize = 8 * 1024 * 1024;

/// Durable whole-work completion progress. Personal library rows are never
/// changed by this pass. The owner and adoption generation bind the checkpoint
/// to the candidate pool that requested it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumContinuation {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    pub pending: usize,
    /// Complete album lists published by this pass.
    pub completed: usize,
    /// Raw positions retained in a newly advanced partial list.
    pub advanced: usize,
    pub retry_after_secs: u64,
    pub requires_refresh: bool,
    cursor: Option<String>,
    partial: Option<PartialAlbum>,
}

impl AlbumContinuation {
    pub(super) fn refresh_required() -> Self {
        Self {
            requires_refresh: true,
            ..Self::default()
        }
    }

    /// A library pass can discover more works without discarding an album
    /// whose earlier pages the background reader already retained.
    pub fn inherit_progress(&mut self, previous: &Self) {
        if !self.requires_refresh
            && self.owner == previous.owner
            && self.generation == previous.generation
        {
            self.cursor.clone_from(&previous.cursor);
            self.partial.clone_from(&previous.partial);
        }
    }

    /// Resume retained pages first; otherwise rotate past the last album
    /// attempted so malformed recordings cannot starve later candidates.
    fn order(&mut self, albums: &[String]) -> Vec<String> {
        let mut order = albums.to_vec();
        if let Some(cursor) = &self.cursor {
            let start = order.partition_point(|uri| uri <= cursor);
            order.rotate_left(start);
        }
        self.partial = self
            .partial
            .take()
            .filter(|part| part.bounded() && albums.contains(&part.uri));
        if let Some(part) = &self.partial
            && let Some(index) = order.iter().position(|uri| uri == &part.uri)
        {
            order.remove(index);
            order.insert(0, part.uri.clone());
        }
        order
    }

    fn matches(&self, owner: &SpotifyLibraryOwner) -> bool {
        self.owner.as_deref() == Some(owner.owner_id.as_str())
            && self.generation == Some(owner.generation)
    }

    fn changed_owner(&mut self) {
        self.requires_refresh = true;
        self.partial = None;
        self.pending = 0;
        self.advanced = 0;
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartialAlbum {
    uri: String,
    offset: u32,
    total: Option<u32>,
    pages: usize,
    tracks: Vec<PartialTrack>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct PartialTrack {
    disc_number: u32,
    track_number: u32,
    source_uri: String,
    title: String,
    duration_secs: Option<u32>,
}

impl From<AlbumTrack> for PartialTrack {
    fn from(track: AlbumTrack) -> Self {
        Self {
            disc_number: track.disc_number,
            track_number: track.track_number,
            source_uri: track.source_uri,
            title: track.title,
            duration_secs: track.duration_secs,
        }
    }
}

impl From<PartialTrack> for AlbumTrack {
    fn from(track: PartialTrack) -> Self {
        Self {
            disc_number: track.disc_number,
            track_number: track.track_number,
            source_uri: track.source_uri,
            title: track.title,
            duration_secs: track.duration_secs,
        }
    }
}

impl PartialAlbum {
    fn new(uri: String) -> Self {
        Self {
            uri,
            offset: 0,
            total: None,
            pages: 0,
            tracks: Vec::new(),
        }
    }

    fn bounded(&self) -> bool {
        album_id(&self.uri).is_some()
            && self.pages <= MAX_ALBUM_PAGES
            && self.offset as usize <= MAX_ALBUM_ENTRIES
            && self.tracks.len() <= self.offset as usize
            && self.total.is_none_or(|total| self.offset <= total)
            && self
                .tracks
                .iter()
                .try_fold(0usize, |bytes, track| {
                    bytes
                        .checked_add(track.source_uri.len())?
                        .checked_add(track.title.len())?
                        .checked_add(64)
                })
                .is_some_and(|bytes| bytes <= MAX_PARTIAL_BYTES)
    }

    /// A validated page is retained atomically; errors discard this album's
    /// partial observation, never publish a truncated complete cache.
    fn ingest(&mut self, body: &[u8], endpoints: &Endpoints) -> Result<bool, SpotifyError> {
        let id = album_id(&self.uri)
            .ok_or_else(|| SpotifyError::Decode("invalid album checkpoint".into()))?;
        if self.pages >= MAX_ALBUM_PAGES {
            return Err(SpotifyError::Decode("album page budget exceeded".into()));
        }
        let url = endpoints.album_tracks(id, self.offset);
        let page = Page::<SimplifiedTrack>::read(body, &url, endpoints, self.total, |offset| {
            endpoints.album_tracks(id, offset)
        })?;
        let count = u32::try_from(page.items.len())
            .map_err(|_| SpotifyError::Decode("album page is too large".into()))?;
        self.offset = self
            .offset
            .checked_add(count)
            .ok_or_else(|| SpotifyError::Decode("album position overflow".into()))?;
        self.total = page.coverage.total;
        self.pages += 1;
        self.tracks.extend(
            page.items
                .iter()
                .filter_map(album_track)
                .map(PartialTrack::from),
        );
        if !self.bounded() {
            return Err(SpotifyError::Decode(
                "album checkpoint budget exceeded".into(),
            ));
        }
        Ok(page.next.is_none())
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Missing lists, excluding albums whose established miss backoff still runs.
/// Sorting makes the durable cursor independent of library row order.
fn missing<S: Store + ?Sized>(store: &S, at: i64) -> Result<Vec<String>, SpotifyError> {
    let works = group_works(pool_from_store(store)?.tracks());
    let mut albums: Vec<String> = works
        .iter()
        .filter(|work| work.needs_expansion())
        .filter_map(|work| work.album_uri.clone())
        .filter(|uri| album_id(uri).is_some())
        .collect();
    albums.sort_unstable();
    albums.dedup();
    let cached: HashSet<String> = store
        .album_tracks_many(&albums.iter().map(String::as_str).collect::<Vec<_>>())?
        .into_iter()
        .map(|(uri, _)| uri)
        .collect();
    let misses = AlbumMisses::load(store, at)?;
    albums.retain(|uri| !cached.contains(uri) && !misses.waiting(uri, at));
    Ok(albums)
}

/// Discover completion work after a full or playlist-only library update.
pub(super) fn status<S: Store + ?Sized>(
    store: &S,
    receipt: &SpotifyLibraryOwner,
    at: i64,
) -> Result<AlbumContinuation, SpotifyError> {
    let mut state = AlbumContinuation {
        owner: Some(receipt.owner_id.clone()),
        generation: Some(receipt.generation),
        ..AlbumContinuation::default()
    };
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
        return Ok(state);
    }
    state.pending = missing(store, at)?.len();
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    }
    Ok(state)
}

/// Complete cached candidate works in a bounded background pass. A legacy
/// scheduler can pass no checkpoint to discover unfinished albums immediately.
/// Reads only account identity and missing album pages, never saved music,
/// playlist directories, taste endpoints or speakers.
pub async fn continue_albums<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    previous: Option<&AlbumContinuation>,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    let at = now();
    let receipt = store.spotify_library_owner()?;
    let mut state = previous.cloned().unwrap_or_default();
    state.completed = 0;
    state.advanced = 0;
    state.retry_after_secs = 0;
    let mut notes = Vec::new();
    if let Some(receipt) = receipt.as_ref() {
        if previous.is_some_and(|saved| !saved.matches(receipt)) {
            state.changed_owner();
        } else {
            state.owner = Some(receipt.owner_id.clone());
            state.generation = Some(receipt.generation);
            state.requires_refresh = false;
            let albums = missing(store, at)?;
            if albums.is_empty() {
                state.pending = 0;
                state.partial = None;
            } else if session.read_owner(cx).await? != receipt.owner_id {
                state.changed_owner();
            } else {
                read_pages(session, cx, store, receipt, &albums, &mut state, &mut notes).await?;
            }
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
            } else if !state.requires_refresh {
                state.pending = missing(store, at)?.len();
            }
        }
    } else {
        state.changed_owner();
    }
    state.retry_after_secs = session.retry_after_secs();
    let library = summarize(&candidate_items(store)?);
    if store.spotify_library_owner()? != receipt {
        state.changed_owner();
    }
    if state.requires_refresh {
        notes.push(
            "album completion needs the current Spotify account's full library refresh".into(),
        );
    }
    Ok(LibrarySyncDetailed {
        library,
        genres: GenreRead::default(),
        playlists: PlaylistContinuation::default(),
        albums: state,
        notes,
    })
}

async fn read_pages<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    receipt: &SpotifyLibraryOwner,
    albums: &[String],
    state: &mut AlbumContinuation,
    notes: &mut Vec<String>,
) -> Result<(), SpotifyError> {
    let at = now();
    let before = state
        .partial
        .as_ref()
        .map(|part| (part.uri.clone(), part.offset));
    let order = state.order(albums);
    let mut misses = AlbumMisses::load(store, at)?;
    let mut requests = 0;
    'albums: for uri in order {
        if requests >= ALBUM_PAGE_BUDGET {
            break;
        }
        state.cursor = Some(uri.clone());
        let mut part = state
            .partial
            .take()
            .filter(|part| part.uri == uri)
            .unwrap_or_else(|| PartialAlbum::new(uri.clone()));
        loop {
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
                break 'albums;
            }
            if requests >= ALBUM_PAGE_BUDGET {
                state.partial = Some(part);
                break 'albums;
            }
            let id = album_id(&uri)
                .ok_or_else(|| SpotifyError::Decode("invalid pending album".into()))?;
            let url = session.endpoints().album_tracks(id, part.offset);
            requests += 1;
            let result = session.get_brief(cx, &url).await;
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
                break 'albums;
            }
            let page = result.and_then(|body| part.ingest(&body, session.endpoints()));
            match page {
                Ok(false) => {}
                Ok(true) => {
                    if part.tracks.is_empty() {
                        misses.record(&uri, at);
                        notes.push(format!("{uri}: no playable album tracks"));
                    } else {
                        let mut tracks: Vec<AlbumTrack> =
                            part.tracks.into_iter().map(AlbumTrack::from).collect();
                        tracks.sort_by_key(|track| (track.disc_number, track.track_number));
                        match store.save_album_tracks(&uri, &tracks, at) {
                            Ok(()) => state.completed += 1,
                            Err(error) => notes.push(format!("{uri}: {error}")),
                        }
                    }
                    break;
                }
                Err(error) if halts(&error) => {
                    state.partial = Some(part);
                    notes.push(format!("album completion paused: {error}"));
                    break 'albums;
                }
                Err(error) => {
                    if !matches!(error, SpotifyError::Decode(_)) {
                        misses.record(&uri, at);
                    }
                    // Move past malformed albums, with no partial cache. Their
                    // next turn starts cleanly after every later album's turn.
                    notes.push(format!("{uri}: {error}"));
                    break;
                }
            }
        }
    }
    if !state.requires_refresh && store.spotify_library_owner()?.as_ref() == Some(receipt) {
        misses.save(store, at)?;
        if let Some(part) = &state.partial {
            let old = before
                .filter(|(uri, _)| uri == &part.uri)
                .map_or(0, |(_, offset)| offset);
            state.advanced = part.offset.saturating_sub(old) as usize;
        }
    } else {
        state.changed_owner();
    }
    Ok(())
}
