//! Finish and revalidate whole-work album metadata independently of the daily
//! library read. Completed lists are refreshed after a week.
//! An unfinished album stays in the private scheduler checkpoint. Only a
//! complete, validated list enters the album cache the selector consumes.

use super::LibrarySyncDetailed;
use super::playlists::{candidate_items, summarize};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::{AlbumMisses, album_id, album_recheck_at, halts};
use crate::genres::GenreRead;
use crate::library::LibraryItem;
use crate::session::Session;
use crate::taste::PlaylistContinuation;
use crate::works::group_works;
use asupersync::Cx;
use fsonos_core::store::{SpotifyLibraryOwner, Store};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

mod library;
mod proof;
pub(super) use library::complete_verified;
use proof::PartialAlbum;
#[cfg(test)]
mod tests;

/// Catalog pages per background pass, plus one account identity read.
pub const ALBUM_PAGE_BUDGET: usize = 8;

/// Durable whole-work completion progress. Personal library rows are never
/// changed by this pass. The owner and adoption generation bind the checkpoint
/// to the candidate pool that requested it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AlbumContinuation {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    pub pending: usize,
    /// Earliest future cache-expiry or known-miss retry, even with no pending
    /// requests. None means there are no future album checks to schedule.
    pub recheck_at: Option<i64>,
    /// Complete catalog observations published by this pass, including a
    /// validated empty or missing album that retires its former cached list.
    pub completed: usize,
    /// Raw positions newly retained or verified in this pass's partial list.
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

    /// A status-only observation can discover more works without discarding
    /// an album's retained pages. A pass that actually read albums already
    /// carries its current cursor and must never be rolled back by this merge.
    pub fn inherit_progress(&mut self, previous: &Self) {
        if !self.requires_refresh
            && !previous.requires_refresh
            && self.owner == previous.owner
            && self.generation == previous.generation
            && self.pending > 0
            && self.completed == 0
            && self.advanced == 0
            && self.cursor.is_none()
            && self.partial.is_none()
        {
            self.cursor.clone_from(&previous.cursor);
            self.partial.clone_from(&previous.partial);
        }
    }

    /// Resume retained pages first; otherwise rotate past the last album
    /// attempted so malformed recordings cannot starve later candidates.
    fn order(&mut self, albums: &[String], at: i64) -> Vec<String> {
        let mut order = albums.to_vec();
        if let Some(cursor) = &self.cursor {
            let start = order.partition_point(|uri| uri <= cursor);
            order.rotate_left(start);
        }
        // Retained metadata must match its bounded, unexpired proof before
        // its cursor can be resumed. Legacy unproved checkpoints start again.
        self.partial = self
            .partial
            .take()
            .and_then(|mut part| (albums.contains(&part.uri) && part.resume(at)).then_some(part));
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
        self.recheck_at = None;
        self.advanced = 0;
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// One candidate/cache snapshot provides both immediate work and its next
/// deadline. Scheduling must not rescan the whole library just for expiry.
struct AlbumInventory {
    pending: Vec<String>,
    recheck_at: Option<i64>,
}

impl AlbumInventory {
    fn observe(&self, state: &mut AlbumContinuation) {
        state.pending = self.pending.len();
        state.recheck_at = self.recheck_at;
    }
}

fn inventory<S: Store + ?Sized>(store: &S, at: i64) -> Result<AlbumInventory, SpotifyError> {
    inventory_for_items(store, &candidate_items(store)?, at)
}

/// Missing and expired lists, excluding albums whose established miss backoff
/// still runs. Sorting keeps rotation independent of library row order.
fn inventory_for_items<S: Store + ?Sized>(
    store: &S,
    items: &[LibraryItem],
    at: i64,
) -> Result<AlbumInventory, SpotifyError> {
    let works = group_works(CandidatePool::build(items).tracks());
    let mut albums: Vec<String> = works
        .iter()
        .filter(|work| work.needs_expansion())
        .filter_map(|work| work.album_uri.clone())
        .filter(|uri| album_id(uri).is_some())
        .collect();
    albums.sort_unstable();
    albums.dedup();
    let cached: HashMap<_, _> = store
        .album_tracks_many(&albums.iter().map(String::as_str).collect::<Vec<_>>())?
        .into_iter()
        .collect();
    let misses = AlbumMisses::load(store, at)?;
    let mut observed = AlbumInventory {
        pending: Vec::new(),
        recheck_at: None,
    };
    for uri in albums {
        let due = cached
            .get(&uri)
            .map_or(at, |album| album_recheck_at(album, at))
            .max(misses.retry_at(&uri).unwrap_or(at));
        if due <= at {
            observed.pending.push(uri);
        } else {
            observed.recheck_at = Some(observed.recheck_at.map_or(due, |next| next.min(due)));
        }
    }
    Ok(observed)
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
    inventory(store, at)?.observe(&mut state);
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    }
    Ok(state)
}

/// Discover newly classical works from the same enriched candidate snapshot
/// used by a genre continuation, without rereading the whole artist cache.
pub(super) fn status_for_items<S: Store + ?Sized>(
    store: &S,
    receipt: &SpotifyLibraryOwner,
    items: &[LibraryItem],
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
    inventory_for_items(store, items, at)?.observe(&mut state);
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    }
    Ok(state)
}

/// Complete or refresh candidate works in a bounded background pass. A legacy
/// scheduler can pass no checkpoint to discover due albums immediately. Reads
/// only account identity and due album pages, never saved music, playlist
/// directories, taste endpoints or speakers.
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
            let albums = inventory(store, at)?;
            albums.observe(&mut state);
            if albums.pending.is_empty() {
                state.partial = None;
            } else if session.read_owner(cx).await? != receipt.owner_id {
                state.changed_owner();
            } else {
                read_pages(
                    session,
                    cx,
                    store,
                    receipt,
                    &albums.pending,
                    &mut state,
                    &mut notes,
                )
                .await?;
            }
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
            }
        }
    } else {
        state.changed_owner();
    }
    state.retry_after_secs = session.retry_after_secs();
    let items = candidate_items(store)?;
    let library = summarize(&items);
    if !state.requires_refresh {
        inventory_for_items(store, &items, now())?.observe(&mut state);
    }
    if store.spotify_library_owner()? != receipt {
        state.changed_owner();
    }
    if state.requires_refresh {
        notes.push(
            "album completion needs the current Spotify account's full library refresh".into(),
        );
    }
    Ok(LibrarySyncDetailed {
        artist_samples: super::ArtistContinuation::default(),
        library,
        genres: GenreRead::default(),
        genre_continuation: super::GenreContinuation::default(),
        playlists: PlaylistContinuation::default(),
        followed_artists: crate::taste::FollowedArtistContinuation::default(),
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
    let order = state.order(albums, at);
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
            .unwrap_or_else(|| PartialAlbum::new(uri.clone(), at));
        // One root check per resumed pass leaves seven requests for forward
        // intake or verification. Persisted verification never starts over
        // merely because this pass used its request allowance.
        let mut root_pending = part.has_pages();
        let mut advanced = 0usize;
        loop {
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
                break 'albums;
            }
            if !root_pending && part.ready() {
                if publish_complete(store, &part, at, &mut misses, state, notes) {
                    break;
                }
                state.partial = Some(part);
                state.advanced = advanced;
                break 'albums;
            }
            if requests >= ALBUM_PAGE_BUDGET {
                state.partial = Some(part);
                state.advanced = advanced;
                break 'albums;
            }
            let check = if root_pending {
                Some(0)
            } else {
                part.verification_index()
            };
            let offset = check
                .and_then(|index| part.page_offset(index))
                .unwrap_or(part.offset);
            let id = album_id(&uri)
                .ok_or_else(|| SpotifyError::Decode("invalid pending album".into()))?;
            let url = session.endpoints().album_tracks(id, offset);
            requests += 1;
            let result = session.get_brief(cx, &url).await;
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
                break 'albums;
            }
            let previous = (part.offset, part.verified_positions());
            let page = result.and_then(|body| match check {
                Some(index) => part.verify(index, &body, session.endpoints()),
                None => part.ingest(&body, session.endpoints()).map(|()| true),
            });
            match page {
                Ok(true) => {
                    root_pending = false;
                    advanced += part.offset.saturating_sub(previous.0) as usize
                        + part.verified_positions().saturating_sub(previous.1);
                }
                Ok(false) => {
                    notes.push(format!(
                        "{uri}: album catalog changed; retained pages discarded"
                    ));
                    // Rotate immediately. A repeatedly changing large album
                    // must not consume every pass ahead of stable recordings.
                    break;
                }
                Err(error) if halts(&error) => {
                    state.partial = Some(part);
                    state.advanced = advanced;
                    notes.push(format!("album completion paused: {error}"));
                    break 'albums;
                }
                Err(error) => {
                    if matches!(
                        error,
                        SpotifyError::Api {
                            status: 404 | 410, ..
                        }
                    ) {
                        // A definite disappearance replaces the old list too.
                        // A failed cache write keeps it due and makes no miss.
                        publish_missing(store, &uri, at, &mut misses, state, notes);
                    }
                    // Malformed or rejected reads cannot replace the last
                    // completed list or establish a week-long negative cache.
                    // Give later albums a turn before this one starts again.
                    notes.push(format!("{uri}: {error}"));
                    break;
                }
            }
        }
    }
    if !state.requires_refresh && store.spotify_library_owner()?.as_ref() == Some(receipt) {
        misses.save(store, at)?;
    } else {
        state.changed_owner();
    }
    Ok(())
}

/// Return false only when the completed metadata must remain private for a
/// later cache-write attempt. Owner and proof checks precede this call.
fn publish_complete<S: Store + ?Sized>(
    store: &mut S,
    part: &PartialAlbum,
    at: i64,
    misses: &mut AlbumMisses,
    state: &mut AlbumContinuation,
    notes: &mut Vec<String>,
) -> bool {
    let uri = &part.uri;
    let mut tracks = part.album_tracks();
    tracks.sort_by_key(|track| (track.disc_number, track.track_number));
    match store.save_album_tracks(uri, &tracks, at) {
        Ok(()) => {
            if tracks.is_empty() {
                misses.record(uri, at);
                notes.push(format!("{uri}: no playable album tracks"));
            }
            state.completed += 1;
            true
        }
        Err(error) => {
            notes.push(format!("{uri}: {error}"));
            false
        }
    }
}

fn publish_missing<S: Store + ?Sized>(
    store: &mut S,
    uri: &str,
    at: i64,
    misses: &mut AlbumMisses,
    state: &mut AlbumContinuation,
    notes: &mut Vec<String>,
) {
    match store.save_album_tracks(uri, &[], at) {
        Ok(()) => {
            misses.record(uri, at);
            state.completed += 1;
        }
        Err(error) => notes.push(format!(
            "{uri}: could not retire the missing album cache: {error}"
        )),
    }
}
