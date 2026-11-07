//! Album completion at the end of an accepted library or playlist window.
//! It uses the same page allowance and checkpoint as dedicated album passes.

use super::{AlbumContinuation, missing, now, read_pages};
use crate::SpotifyError;
use crate::session::Session;
use asupersync::Cx;
use fsonos_core::store::{SpotifyLibraryOwner, Store};

/// The caller already verified the account and published through this receipt.
/// A new adoption discards old album progress; a same-owner refresh resumes it.
pub(in crate::cache) async fn complete_verified<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    receipt: &SpotifyLibraryOwner,
    previous: Option<&AlbumContinuation>,
    notes: &mut Vec<String>,
) -> Result<AlbumContinuation, SpotifyError> {
    let at = now();
    let mut state = previous
        .filter(|saved| !saved.requires_refresh && saved.matches(receipt))
        .cloned()
        .unwrap_or_default();
    state.owner = Some(receipt.owner_id.clone());
    state.generation = Some(receipt.generation);
    state.completed = 0;
    state.advanced = 0;
    state.retry_after_secs = 0;
    state.requires_refresh = false;
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    } else {
        let albums = missing(store, at)?;
        if store.spotify_library_owner()?.as_ref() != Some(receipt) {
            state.changed_owner();
        } else if albums.is_empty() {
            state.pending = 0;
            state.partial = None;
        } else if session.retry_after_secs() > 0 {
            notes.push("album completion paused: the Spotify API cooldown is still active".into());
        } else {
            read_pages(session, cx, store, receipt, &albums, &mut state, notes).await?;
        }
        if store.spotify_library_owner()?.as_ref() != Some(receipt) {
            state.changed_owner();
        } else if !state.requires_refresh {
            state.pending = missing(store, at)?.len();
        }
    }
    if store.spotify_library_owner()?.as_ref() != Some(receipt) {
        state.changed_owner();
    }
    state.retry_after_secs = session.retry_after_secs();
    if state.requires_refresh {
        notes.push(
            "album completion needs the current Spotify account's full library refresh".into(),
        );
    }
    Ok(state)
}
