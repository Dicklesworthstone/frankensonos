//! Continue durable playlist windows without a complete saved-library reread.

use super::{
    LibrarySync, LibrarySyncDetailed, acknowledge_playlists, complete_library_works, from_entry,
    publish_library_read, sync_notes,
};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::session::Session;
use crate::taste::{PlaylistContinuation, Taste, TasteRead, read_pending_playlists};
use crate::works::group_works;
use asupersync::Cx;
use fsonos_core::store::Store;
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Finish only unread or unfinished playlist snapshots for the last full
/// sync's owner. It reads the directory and at most 25 bounded windows; no
/// saved albums/likes, top items, artist endpoints or speaker calls occur.
/// Existing candidates and their weights survive this additive enrichment.
/// Only a full sync has authority to retire music absent from the account.
/// Newly observed works use the same best-effort album completion as a full
/// sync, so a movement on a late playlist page need not play alone all day.
pub async fn continue_playlists<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    if owner.trim().is_empty() || owner.len() > 1_024 || owner.chars().any(char::is_control) {
        return Err(SpotifyError::Config(
            "playlist continuation needs a validated Spotify owner".into(),
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
    let receipt = store.spotify_library_owner()?;
    let (taste, mut read) = if receipt
        .as_ref()
        .is_some_and(|receipt| receipt.owner_id == owner)
    {
        read_pending_playlists(session, cx, store, owner, now).await
    } else {
        // A scheduler checkpoint alone is not proof of the library's owner:
        // another process may have switched accounts, or this is a legacy DB.
        (
            Taste::default(),
            TasteRead {
                continuation: PlaylistContinuation {
                    owner: receipt.as_ref().map(|receipt| receipt.owner_id.clone()),
                    requires_refresh: true,
                    ..PlaylistContinuation::default()
                },
                failed: vec![(
                    "playlist continuation".into(),
                    "the library's Spotify owner needs a full refresh".into(),
                )],
                ..TasteRead::default()
            },
        )
    };
    let mut items = candidate_items(store)?;
    let mut library = summarize(&items);
    let mut albums = Expansion::default();
    if let Some(receipt) = receipt.filter(|_| !read.continuation.requires_refresh) {
        let incoming = incoming_items(store, &taste)?;
        items.extend(incoming);
        if let Some(applied) = publish_library_read(store, &receipt, &items, &taste)? {
            library = applied;
            acknowledge_playlists(store, &receipt, &taste, &mut read.continuation)?;
            // Complete only works touched by this pass, including movements
            // cached from an earlier window, within the usual read bounds.
            let observed: HashSet<&str> = taste
                .tracks
                .iter()
                .map(|item| item.source_uri.as_str())
                .collect();
            let works = group_works(CandidatePool::build(&items).tracks())
                .into_iter()
                .filter(|work| {
                    work.movements
                        .iter()
                        .any(|track| observed.contains(track.track.source_uri.as_str()))
                })
                .collect();
            albums = complete_library_works(session, cx, store, works, now).await;
        } else {
            // Even a switch away and back invalidates the original receipt.
            // Keep the new adoption's cache and leave this pass unacknowledged.
            read.continuation.requires_refresh = true;
            read.continuation.owner = store.spotify_library_owner()?.map(|owner| owner.owner_id);
            read.failed.push((
                "playlist continuation".into(),
                "the Spotify account changed during this pass; a full refresh is needed".into(),
            ));
            library = summarize(&candidate_items(store)?);
        }
    }
    let genres = GenreRead::default();
    let notes = sync_notes(&read, &genres, &albums);
    let mut playlists = read.continuation;
    playlists.retry_after_secs = session.retry_after_secs();
    Ok(LibrarySyncDetailed {
        library,
        genres,
        playlists,
        notes,
    })
}

fn candidate_items<S: Store + ?Sized>(
    store: &S,
) -> Result<Vec<crate::library::LibraryItem>, SpotifyError> {
    Ok(store
        .library()?
        .iter()
        .filter(|entry| entry.is_candidate())
        .map(from_entry)
        .collect())
}

fn summarize(items: &[crate::library::LibraryItem]) -> LibrarySync {
    let pool = CandidatePool::build(items);
    let candidates = pool.tracks().iter().filter(|track| !track.explicit);
    LibrarySync {
        tracks: items.len(),
        candidates: candidates.clone().count(),
        classical: candidates.filter(|track| track.classical).count(),
        retired: 0,
    }
}

fn incoming_items<S: Store + ?Sized>(
    store: &S,
    taste: &Taste,
) -> Result<Vec<crate::library::LibraryItem>, SpotifyError> {
    let mut incoming = taste.tracks.clone();
    taste.weigh(&mut incoming);
    taste.name_genres(&mut incoming);
    // Reuse stored genre evidence without adding artist requests to this pass.
    for item in &mut incoming {
        if item.genres.is_empty()
            && let Some(id) = &item.artist_id
            && let Some((genres, _)) = store.artist_genres(id)?
        {
            item.genres = genres;
        }
    }
    crate::genre_guess::fill(&mut incoming);
    Ok(incoming)
}
