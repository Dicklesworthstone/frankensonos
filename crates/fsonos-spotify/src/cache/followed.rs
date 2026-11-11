//! Continue followed-artist membership without rereading saved music.

use super::{LibrarySyncDetailed, playlists, sync_notes};
use crate::SpotifyError;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::session::Session;
use crate::taste::{FollowedArtistContinuation, Taste, TasteRead, read_pending_followed};
use asupersync::Cx;
use fsonos_core::store::Store;
use std::time::{SystemTime, UNIX_EPOCH};

/// Read one bounded, resumable followed-artist window for the accepted owner.
/// Only identity and directory pages use the network. Genre evidence is cached
/// separately; candidate rows and taste weights are left to a full refresh.
/// A verified complete directory remains pending until that refresh reconciles
/// all current taste signals. No saved-library proof is consumed here.
pub async fn continue_followed_artists<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    if owner.trim().is_empty() || owner.len() > 1_024 || owner.chars().any(char::is_control) {
        return Err(SpotifyError::Config(
            "followed-artist continuation needs a validated Spotify owner".into(),
        ));
    }
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        });
    let receipt = store.spotify_library_owner()?;
    let (_, mut read) =
        if let Some(receipt) = receipt.as_ref().filter(|receipt| receipt.owner_id == owner) {
            read_pending_followed(session, cx, store, receipt, now).await
        } else {
            (
                Taste::default(),
                TasteRead {
                    followed_continuation: FollowedArtistContinuation::refresh_required(),
                    failed: vec![(
                        "followed artists".into(),
                        "the library's Spotify owner needs a full refresh".into(),
                    )],
                    ..TasteRead::default()
                },
            )
        };
    if store.spotify_library_owner()? != receipt {
        read.followed_continuation = FollowedArtistContinuation::refresh_required();
        read.failed.push((
            "followed artists".into(),
            "the Spotify account changed during this pass; a full refresh is needed".into(),
        ));
    }
    // Only observe the current public library after network I/O. Rewriting a
    // copied snapshot could overwrite a concurrent same-account publication.
    let library = playlists::summarize(&playlists::candidate_items(store)?);
    let genres = GenreRead::default();
    let notes = sync_notes(&read, &genres, &Expansion::default());
    let mut followed_artists = read.followed_continuation;
    followed_artists.retry_after_secs = session.retry_after_secs();
    Ok(LibrarySyncDetailed {
        artist_samples: super::ArtistContinuation::default(),
        library,
        genres,
        genre_continuation: super::GenreContinuation::default(),
        playlists: crate::taste::PlaylistContinuation::default(),
        followed_artists,
        albums: super::AlbumContinuation::default(),
        notes,
    })
}
