//! Finish artist metadata without repeating the owner's saved-library read.
//! The scheduler retains an owner-bound cursor; accepted artist tags live in
//! the public catalog cache and are overlaid when the DJ rebuilds its pool.

use super::playlists::{candidate_items, summarize};
use super::{AlbumContinuation, LibrarySyncDetailed, sync_notes};
use crate::SpotifyError;
use crate::expand::{Expansion, halts};
use crate::genres::{GenreRead, REFRESH_SECS};
use crate::session::Session;
use crate::taste::{PlaylistContinuation, TasteRead};
use asupersync::Cx;
use fsonos_core::store::{SpotifyLibraryOwner, Store};
use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

/// Artist reads per background pass, plus one account identity read.
pub const GENRE_READ_BUDGET: usize = 50;

/// Durable artist-genre progress for the adopted personal library. The cursor
/// advances past failures as well as successes, so an unavailable artist cannot
/// repeatedly consume the entire budget before later artists are reached.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GenreContinuation {
    pub owner: Option<String>,
    pub generation: Option<u64>,
    pub pending: usize,
    /// Artist results successfully cached by this pass, including empty tags.
    pub completed: usize,
    pub retry_after_secs: u64,
    pub requires_refresh: bool,
    cursor: Option<String>,
}

impl GenreContinuation {
    pub(super) fn refresh_required() -> Self {
        Self {
            requires_refresh: true,
            ..Self::default()
        }
    }

    /// A full or playlist read can discover new artists without resetting
    /// progress through the same account's previously unfinished artists.
    pub fn inherit_progress(&mut self, previous: &Self) {
        if !self.requires_refresh
            && !previous.requires_refresh
            && self.owner == previous.owner
            && self.generation == previous.generation
        {
            self.cursor.clone_from(&previous.cursor);
        }
    }

    fn order(&self, artists: &[String]) -> Vec<String> {
        let mut order = artists.to_vec();
        if let Some(cursor) = &self.cursor {
            let start = order.partition_point(|id| id <= cursor);
            order.rotate_left(start);
        }
        order
    }

    fn matches(&self, receipt: &SpotifyLibraryOwner) -> bool {
        self.owner.as_deref() == Some(receipt.owner_id.as_str())
            && self.generation == Some(receipt.generation)
    }

    fn changed_owner(&mut self) {
        self.requires_refresh = true;
        self.pending = 0;
        self.completed = 0;
        self.cursor = None;
    }
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

struct Pending {
    artists: Vec<String>,
    cached: usize,
}

/// Only current candidates create work. Missing artist IDs are legitimate
/// metadata gaps, not requests to guess an artist or force endless full syncs.
fn missing<S: Store + ?Sized>(store: &S, at: i64) -> Result<Pending, SpotifyError> {
    let mut artists: Vec<String> = store
        .library()?
        .into_iter()
        .filter(|entry| entry.is_candidate())
        .filter_map(|entry| entry.artist_id)
        .filter(|id| !id.trim().is_empty())
        .collect();
    artists.sort_unstable();
    artists.dedup();
    let mut pending = Vec::new();
    let mut cached = 0;
    let mut failures = 0usize;
    let mut first_failure = None;
    for id in artists {
        let fresh = match store.artist_genres(&id) {
            Ok(cached) => {
                cached.is_some_and(|(_, fetched_at)| at.saturating_sub(fetched_at) < REFRESH_SECS)
            }
            Err(error) => {
                failures += 1;
                first_failure.get_or_insert(error);
                false
            }
        };
        if fresh {
            cached += 1;
        } else {
            pending.push(id);
        }
    }
    if let Some(error) = first_failure {
        tracing::warn!(
            failed_artists = failures,
            %error,
            "artist genre cache unavailable; keeping those artists pending"
        );
    }
    Ok(Pending {
        artists: pending,
        cached,
    })
}

/// Discover work after an accepted full or playlist-only library update.
pub(super) fn status<S: Store + ?Sized>(
    store: &S,
    receipt: &SpotifyLibraryOwner,
    at: i64,
) -> Result<GenreContinuation, SpotifyError> {
    let mut state = GenreContinuation {
        owner: Some(receipt.owner_id.clone()),
        generation: Some(receipt.generation),
        ..GenreContinuation::default()
    };
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
        return Ok(state);
    }
    state.pending = missing(store, at)?.artists.len();
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    }
    Ok(state)
}

/// Enrich cached candidates in a bounded pass, without reading saved music,
/// playlist directories, taste endpoints or speakers. The scheduler persists
/// the returned cursor and cooldown separately from the full-sync timestamp.
/// Public artist results never replace personal library rows or taste weights.
pub async fn continue_genres<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    previous: Option<&GenreContinuation>,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    let at = now();
    let receipt = store.spotify_library_owner()?;
    let mut state = previous.cloned().unwrap_or_default();
    state.completed = 0;
    state.retry_after_secs = 0;
    let mut genres = GenreRead::default();
    if let Some(receipt) = receipt.as_ref() {
        if previous.is_some_and(|saved| !saved.matches(receipt)) {
            state.changed_owner();
        } else {
            state.owner = Some(receipt.owner_id.clone());
            state.generation = Some(receipt.generation);
            state.requires_refresh = false;
            let pending = missing(store, at)?;
            genres.cached = pending.cached;
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
            } else if !pending.artists.is_empty() {
                if session.retry_after_secs() > 0 {
                    genres.halted = Some("the Spotify API cooldown is still active".into());
                } else {
                    let identity = session.read_owner(cx).await;
                    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                        state.changed_owner();
                    } else {
                        match identity {
                            Ok(owner) if owner == receipt.owner_id => {
                                read_artists(
                                    session,
                                    cx,
                                    store,
                                    receipt,
                                    &pending.artists,
                                    &mut state,
                                    &mut genres,
                                )
                                .await?;
                            }
                            Ok(_) => state.changed_owner(),
                            Err(error) => genres.halted = Some(error.to_string()),
                        }
                    }
                }
            }
            if store.spotify_library_owner()?.as_ref() != Some(receipt) {
                state.changed_owner();
            } else if !state.requires_refresh {
                state.pending = missing(store, at)?.artists.len();
            }
        }
    } else {
        state.changed_owner();
    }
    state.retry_after_secs = session.retry_after_secs();
    let library = summarize(&candidate_items(store)?);
    // Newly learned genres may reveal classical works needing album lists.
    // Discovery is local; album requests retain their separate pass budget.
    let albums = match receipt.as_ref().filter(|_| !state.requires_refresh) {
        Some(receipt) => super::albums::status(store, receipt, at)?,
        None => AlbumContinuation::refresh_required(),
    };
    if store.spotify_library_owner()? != receipt {
        state.changed_owner();
    }
    genres.deferred = state.pending.saturating_sub(genres.failed.len());
    let mut notes = sync_notes(&TasteRead::default(), &genres, &Expansion::default());
    if state.requires_refresh {
        notes.push(
            "genre enrichment needs the current Spotify account's full library refresh".into(),
        );
    }
    Ok(LibrarySyncDetailed {
        library,
        genres,
        genre_continuation: state,
        playlists: PlaylistContinuation::default(),
        albums,
        notes,
    })
}

async fn read_artists<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    receipt: &SpotifyLibraryOwner,
    artists: &[String],
    state: &mut GenreContinuation,
    report: &mut GenreRead,
) -> Result<(), SpotifyError> {
    for id in state.order(artists).into_iter().take(GENRE_READ_BUDGET) {
        if store.spotify_library_owner()?.as_ref() != Some(receipt) {
            state.changed_owner();
            break;
        }
        state.cursor = Some(id.clone());
        let result = session.read_artist_genres(cx, &id).await;
        if store.spotify_library_owner()?.as_ref() != Some(receipt) {
            state.changed_owner();
            break;
        }
        let tags = match result {
            Ok(tags) => tags,
            // Unknown catalog IDs are a completed empty result, just as in
            // the full-sync enrichment path.
            Err(SpotifyError::Api { status: 404, .. }) => Vec::new(),
            Err(error) if halts(&error) => {
                report.halted = Some(error.to_string());
                break;
            }
            Err(error) => {
                report.failed.push((id, error.to_string()));
                continue;
            }
        };
        if let Err(error) = store.save_artist_genres(&id, &tags, now()) {
            report.failed.push((id, error.to_string()));
        } else {
            state.completed += 1;
            report.fetched += 1;
        }
    }
    Ok(())
}
