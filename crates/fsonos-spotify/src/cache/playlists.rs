//! Continue durable playlist windows without a complete saved-library reread.

use super::{
    AlbumContinuation, LibrarySync, LibrarySyncDetailed, acknowledge_playlists, from_entry,
    sync_notes, to_entry,
};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::session::Session;
use crate::taste::{PlaylistContinuation, Taste, TasteRead, read_pending_playlists};
use asupersync::Cx;
use fsonos_core::store::{
    ArtistGenresBatch, SpotifyLibraryOwner, SpotifyPlaylistPublication, Store,
};
use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

/// Finish only unread or unfinished playlist snapshots for the last full
/// sync's owner. It reads the directory and at most 25 bounded windows; no
/// saved albums/likes, top items, artist endpoints or speaker calls occur.
/// Existing candidates and their weights survive this additive enrichment.
/// Only a full sync has authority to retire music absent from the account.
/// Album completion shares the full sync's separate bounded page allowance.
pub async fn continue_playlists<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    continue_playlists_with_albums(session, cx, store, owner, None).await
}

/// Finish playlist windows and continue the scheduler's existing album read.
/// A long album resumes its accepted offset even when playlist work or a full
/// refresh happens between its dedicated continuation passes.
pub async fn continue_playlists_with_albums<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
    previous_albums: Option<&AlbumContinuation>,
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
    let (mut taste, mut read) =
        if let Some(receipt) = receipt.as_ref().filter(|receipt| receipt.owner_id == owner) {
            read_pending_playlists(session, cx, store, receipt, now).await
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
    let mut album_notes = Vec::new();
    let mut completion = None;
    if let Some(receipt) = receipt
        .as_ref()
        .filter(|_| !read.continuation.requires_refresh && taste.directory.is_some())
    {
        if publish_playlist_read(store, receipt, &mut taste)? {
            acknowledge_playlists(&taste, &mut read.continuation);
            // A directory proof follows a verified identity read. A missing
            // grant or failed identity cannot authorize unrelated album I/O;
            // an API halt also stops later optional reads in this pass.
            if taste.directory.is_some() && read.halted.is_none() {
                completion = Some(
                    super::albums::complete_verified(
                        session,
                        cx,
                        store,
                        receipt,
                        previous_albums,
                        &mut album_notes,
                    )
                    .await?,
                );
            }
        } else {
            // A newer account, directory or playlist proof owns publication.
            // Keep its cache and leave this pass's observations unacknowledged.
            read.continuation.requires_refresh = true;
            read.continuation.owner = store.spotify_library_owner()?.map(|owner| owner.owner_id);
            read.failed.push((
                "playlist continuation".into(),
                "the Spotify account or accepted playlist read changed during this pass; a full refresh is needed".into(),
            ));
        }
    }
    let genres = GenreRead::default();
    let mut notes = sync_notes(&read, &genres, &Expansion::default());
    notes.extend(album_notes);
    let mut completion = match completion {
        Some(completion) => completion,
        None => match receipt.as_ref() {
            Some(receipt) => super::albums::status(store, receipt, now)?,
            None => AlbumContinuation::refresh_required(),
        },
    };
    if let Some(previous) = previous_albums {
        completion.inherit_progress(previous);
    }
    completion.retry_after_secs = session.retry_after_secs();
    let mut genre_continuation = match receipt.as_ref() {
        Some(receipt) => super::genre_completion::status(store, receipt, now)?,
        None => super::GenreContinuation::refresh_required(),
    };
    genre_continuation.retry_after_secs = session.retry_after_secs();
    let mut playlists = read.continuation;
    playlists.retry_after_secs = session.retry_after_secs();
    // The accepted transaction can preserve rows published by another process.
    // Report the durable candidate set, never a pre-publication copy.
    let library = summarize(&candidate_items(store)?);
    Ok(LibrarySyncDetailed {
        artist_samples: super::ArtistContinuation::default(),
        library,
        genres,
        genre_continuation,
        playlists,
        followed_artists: crate::taste::FollowedArtistContinuation::default(),
        albums: completion,
        notes,
    })
}

pub(super) fn publish_playlist_read<S: Store + ?Sized>(
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    taste: &mut Taste,
) -> Result<bool, SpotifyError> {
    if taste.directory.is_none() {
        return Ok(false);
    }
    let (fence, directory) = crate::taste::prepare_playlist_publication(
        owner,
        taste.directory.as_ref(),
        None,
        &taste.playlist_contents,
    )?;
    let supported: HashSet<&str> = fence
        .observations
        .iter()
        .flat_map(|observation| observation.contents.tracks.iter())
        .map(|track| track.source_uri.as_str())
        .collect();
    let incoming = incoming_items(store, taste)
        .into_iter()
        .filter(|item| supported.contains(item.source_uri.as_str()))
        .collect::<Vec<_>>();
    let incoming = crate::library::merge_duplicates(&incoming);
    let pool = CandidatePool::build(&incoming);
    let entries = incoming
        .iter()
        .filter_map(|item| {
            pool.get(&item.source_uri)
                .map(|candidate| to_entry(item, Some(candidate)))
        })
        .collect();
    let publication = SpotifyPlaylistPublication {
        expected_directory: fence.expected_directory,
        directory: fence.directory,
        playlists: taste.playlists.clone(),
        entries,
        observations: fence.observations,
    };
    if store.publish_spotify_playlist_continuation(owner, &publication)? {
        taste.directory = directory;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub(super) fn candidate_items<S: Store + ?Sized>(
    store: &S,
) -> Result<Vec<crate::library::LibraryItem>, SpotifyError> {
    candidate_items_with_genres(store).map(|(items, _)| items)
}

pub(super) fn candidate_items_with_genres<S: Store + ?Sized>(
    store: &S,
) -> Result<(Vec<crate::library::LibraryItem>, ArtistGenresBatch), SpotifyError> {
    let mut items = store
        .library()?
        .iter()
        .filter(|entry| entry.is_candidate())
        .map(from_entry)
        .collect::<Vec<_>>();
    let genres = crate::genres::cached_genres(store, &items);
    crate::genres::apply_genres(&genres, &mut items);
    Ok((items, genres))
}

pub(super) fn summarize(items: &[crate::library::LibraryItem]) -> LibrarySync {
    let pool = CandidatePool::build(items);
    let candidates = pool.tracks().iter().filter(|track| !track.explicit);
    LibrarySync {
        tracks: items.len(),
        candidates: candidates.clone().count(),
        classical: candidates.filter(|track| track.classical).count(),
        retired: 0,
    }
}

fn incoming_items<S: Store + ?Sized>(store: &S, taste: &Taste) -> Vec<crate::library::LibraryItem> {
    let mut incoming = taste.tracks.clone();
    taste.weigh(&mut incoming);
    taste.name_genres(&mut incoming);
    let genres = crate::genres::cached_genres(store, &incoming);
    crate::genres::apply_genres(&genres, &mut incoming);
    crate::genre_guess::fill(&mut incoming);
    incoming
}
