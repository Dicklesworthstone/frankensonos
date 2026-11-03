//! Continue durable playlist windows without a complete saved-library reread.

use super::{
    LibrarySync, LibrarySyncDetailed, acknowledge_playlists, apply_library_read,
    complete_library_works, from_entry, pool_from_store, publish_playlists, sync_notes,
};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::session::Session;
use crate::taste::read_pending_playlists;
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
    let (taste, mut read) = read_pending_playlists(session, cx, store, owner, now).await;
    let mut items = store
        .library()?
        .iter()
        .filter(|entry| entry.is_candidate())
        .map(from_entry)
        .collect::<Vec<_>>();
    let (library, albums) = if read.continuation.requires_refresh {
        // Never mix a newly signed-in account into an old owner's candidates.
        let pool = CandidatePool::build(&items);
        let candidates = pool.tracks().iter().filter(|track| !track.explicit);
        (
            LibrarySync {
                tracks: items.len(),
                candidates: candidates.clone().count(),
                classical: candidates.filter(|track| track.classical).count(),
                retired: 0,
            },
            Expansion::default(),
        )
    } else {
        publish_playlists(store, &taste)?;
        let mut incoming = taste.tracks.clone();
        taste.weigh(&mut incoming);
        taste.name_genres(&mut incoming);
        // Reuse genre evidence already in the store without new artist GETs.
        for item in &mut incoming {
            if item.genres.is_empty()
                && let Some(id) = &item.artist_id
                && let Some((genres, _)) = store.artist_genres(id)?
            {
                item.genres = genres;
            }
        }
        crate::genre_guess::fill(&mut incoming);
        items.extend(incoming);
        let applied = apply_library_read(store, &items)?;
        acknowledge_playlists(store, &taste, &mut read.continuation)?;
        // Complete only works touched by this pass, including any movements
        // already cached from an earlier window. Unrelated incomplete albums
        // must not turn a bounded continuation into another full library read.
        let observed: HashSet<&str> = taste
            .tracks
            .iter()
            .map(|item| item.source_uri.as_str())
            .collect();
        let works = if observed.is_empty() {
            Vec::new()
        } else {
            group_works(pool_from_store(store)?.tracks())
                .into_iter()
                .filter(|work| {
                    work.movements
                        .iter()
                        .any(|track| observed.contains(track.track.source_uri.as_str()))
                })
                .collect()
        };
        let albums = complete_library_works(session, cx, store, works, now).await;
        (applied, albums)
    };
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
