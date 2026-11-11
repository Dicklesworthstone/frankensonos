//! Complete a finite artist-sampling cohort without rereading saved music.
//! Samples and the remaining backlog publish in one account/receipt-fenced
//! transaction. A background pass never retires existing music.

use super::{ArtistContinuation, LibrarySyncDetailed, playlists, sync_notes, to_entry};
use crate::SpotifyError;
use crate::classical::CandidatePool;
use crate::expand::Expansion;
use crate::genres::GenreRead;
use crate::library::{LibraryItem, merge_duplicates};
use crate::session::Session;
use crate::taste::{ArtistDiscovery, TasteRead, read_pending_artist_samples};
use asupersync::Cx;
use fsonos_core::store::{LibraryEntry, SpotifyLibraryOwner, Store};
use std::time::{SystemTime, UNIX_EPOCH};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

/// Compact status from the same accepted receipt that authorizes continuation.
/// Legacy cursors need a new full refresh to supply a finite cohort.
pub(super) fn status<S: Store + ?Sized>(
    store: &S,
    owner: &SpotifyLibraryOwner,
    at: i64,
) -> Result<ArtistContinuation, SpotifyError> {
    let raw = store.spotify_artist_discovery(owner)?;
    let state = raw
        .as_deref()
        .and_then(|raw| ArtistDiscovery::decode(raw).ok())
        .map_or_else(ArtistContinuation::refresh_required, |progress| {
            progress.report(owner, at)
        });
    Ok(if store.spotify_library_owner()?.as_ref() == Some(owner) {
        state
    } else {
        ArtistContinuation::refresh_required()
    })
}

/// Attempt at most fifty artists and retain at most three playable tracks each.
/// The cohort comes from an accepted full refresh. Only artist catalog and
/// before/after account identity requests occur; an empty or limited cohort
/// sends no requests. Completed counts accumulate within that finite cycle.
pub async fn continue_artist_samples<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    previous: Option<&ArtistContinuation>,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    let at = now();
    let owner = store.spotify_library_owner()?;
    let mut state = ArtistContinuation::refresh_required();
    let mut read = TasteRead::default();
    if let Some(owner) = &owner
        && previous.is_none_or(|saved| saved.matches(owner))
        && let Some(raw) = store.spotify_artist_discovery(owner)?
    {
        match ArtistDiscovery::decode(&raw) {
            Ok(mut progress) => {
                state = progress.report(owner, at);
                if !state.requires_refresh && !state.limited && state.pending > 0 {
                    sample_cohort(
                        session, cx, store, owner, &raw, &mut progress, &mut state, &mut read,
                    )
                    .await?;
                }
            }
            Err(error) => read
                .failed
                .push(("artist discovery progress".into(), error.to_string())),
        }
    }
    if store.spotify_library_owner()? != owner {
        state = ArtistContinuation::refresh_required();
    }
    state.retry_after_secs = session.retry_after_secs();
    report(store, owner.as_ref(), state, &read, at)
}

#[allow(
    clippy::too_many_arguments,
    reason = "one bounded pass shares the exact owner and publication receipts"
)]
async fn sample_cohort<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &SpotifyLibraryOwner,
    raw: &str,
    progress: &mut ArtistDiscovery,
    state: &mut ArtistContinuation,
    read: &mut TasteRead,
) -> Result<(), SpotifyError> {
    if !progress.permits_scopes(&session.missing_taste_scopes()) {
        *state = ArtistContinuation::refresh_required();
        return Ok(());
    }
    if session.retry_after_secs() > 0 {
        read.halted = Some("the Spotify API cooldown is still active".into());
        return Ok(());
    }
    if !verify_identity(session, cx, store, owner, raw, state, read).await? {
        return Ok(());
    }
    let selected = progress.selection();
    let (samples, observed) = read_pending_artist_samples(session, cx, owner, &selected).await;
    *read = observed;
    state.attempted = read.artist_reads;
    // A long rate limit also prevents the closing identity read. Keep the exact
    // prior backlog in that case; no unconfirmed personal evidence is published.
    if !verify_identity(session, cx, store, owner, raw, state, read).await? {
        return Ok(());
    }
    progress.accepted(&samples.successful, samples.last_attempted.as_deref());
    let mut tracks = samples.tracks;
    progress.weigh(&mut tracks);
    let entries = prepare_entries(store, &tracks);
    let fence = progress.publication(raw)?;
    if store.publish_spotify_artist_continuation(owner, &fence, &entries)? {
        *state = progress.report(owner, now());
        state.attempted = read.artist_reads;
    } else {
        *state = ArtistContinuation::refresh_required();
    }
    Ok(())
}

async fn verify_identity<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &S,
    owner: &SpotifyLibraryOwner,
    raw: &str,
    state: &mut ArtistContinuation,
    read: &mut TasteRead,
) -> Result<bool, SpotifyError> {
    let identity = session.read_owner(cx).await;
    if store.spotify_library_owner()?.as_ref() != Some(owner)
        || store.spotify_artist_discovery(owner)?.as_deref() != Some(raw)
    {
        *state = ArtistContinuation::refresh_required();
        return Ok(false);
    }
    match identity {
        Ok(id) if id == owner.owner_id => Ok(true),
        Ok(_) => {
            *state = ArtistContinuation::refresh_required();
            Ok(false)
        }
        Err(error) => {
            read.halted = Some(format!("artist sampling identity: {error}"));
            Ok(false)
        }
    }
}

fn prepare_entries<S: Store + ?Sized>(store: &S, tracks: &[LibraryItem]) -> Vec<LibraryEntry> {
    let mut tracks = merge_duplicates(tracks);
    let genres = crate::genres::cached_genres(store, &tracks);
    crate::genres::apply_genres(&genres, &mut tracks);
    crate::genre_guess::fill(&mut tracks);
    let pool = CandidatePool::build(&tracks);
    tracks
        .iter()
        .filter_map(|track| {
            pool.get(&track.source_uri)
                .map(|candidate| to_entry(track, Some(candidate)))
        })
        .collect()
}

fn report<S: Store + ?Sized>(
    store: &S,
    owner: Option<&SpotifyLibraryOwner>,
    mut state: ArtistContinuation,
    read: &TasteRead,
    at: i64,
) -> Result<LibrarySyncDetailed, SpotifyError> {
    // Observe current durable candidates after I/O and additive publication;
    // a copied pre-read library cannot overwrite another process's metadata.
    let items = playlists::candidate_items(store)?;
    let current = owner.filter(|_| !state.requires_refresh);
    let mut albums = match current {
        Some(owner) => super::albums::status_for_items(store, owner, &items, at)?,
        None => super::AlbumContinuation::refresh_required(),
    };
    let mut genre_continuation = match current {
        Some(owner) => super::genre_completion::status(store, owner, at)?,
        None => super::GenreContinuation::refresh_required(),
    };
    if store.spotify_library_owner()?.as_ref() != owner {
        state = ArtistContinuation::refresh_required();
        albums = super::AlbumContinuation::refresh_required();
        genre_continuation = super::GenreContinuation::refresh_required();
    }
    albums.retry_after_secs = state.retry_after_secs;
    genre_continuation.retry_after_secs = state.retry_after_secs;
    let genres = GenreRead::default();
    let mut notes = sync_notes(read, &genres, &Expansion::default());
    if state.requires_refresh {
        notes.push(
            "artist sampling needs the current Spotify account's full library refresh".into(),
        );
    } else if state.limited {
        notes.push(
            "artist sampling reached its private metadata limit; automatic samples are stopped"
                .into(),
        );
    }
    Ok(LibrarySyncDetailed {
        library: playlists::summarize(&items),
        genres,
        genre_continuation,
        playlists: super::PlaylistContinuation::default(),
        followed_artists: super::FollowedArtistContinuation::default(),
        artist_samples: state,
        albums,
        notes,
    })
}
