//! Continue durable playlist windows without a complete saved-library reread.

use super::{
    LibrarySync, LibrarySyncDetailed, acknowledge_playlists, apply_library_read, from_entry,
    publish_playlists, sync_notes,
};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::session::Session;
use crate::taste::read_pending_playlists;
use asupersync::Cx;
use fsonos_core::store::Store;
use std::time::{SystemTime, UNIX_EPOCH};

/// Finish only unread or unfinished playlist snapshots for the last full
/// sync's owner. It reads the directory and at most 25 bounded windows; no
/// saved albums/likes, top items, artist endpoints or speaker calls occur.
/// Existing candidates and their weights survive this additive enrichment.
/// Only a full sync has authority to retire music absent from the account.
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
    let library = if read.continuation.requires_refresh {
        // Never mix a newly signed-in account into an old owner's candidates.
        let pool = CandidatePool::build(&items);
        let candidates = pool.tracks().iter().filter(|track| !track.explicit);
        LibrarySync {
            tracks: items.len(),
            candidates: candidates.clone().count(),
            classical: candidates.filter(|track| track.classical).count(),
            retired: 0,
        }
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
        applied
    };
    let genres = GenreRead::default();
    let notes = sync_notes(&read, &genres, &Expansion::default());
    let mut playlists = read.continuation;
    playlists.retry_after_secs = session.retry_after_secs();
    Ok(LibrarySyncDetailed {
        library,
        genres,
        playlists,
        notes,
    })
}
