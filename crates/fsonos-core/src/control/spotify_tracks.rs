//! Render a bounded Spotify catalog expansion through one household account.
//! Known tracks share queue insertion proofs and staged source replacement.

use super::{Queued, SourceDestination, addr, pause_confirmed, queue_insert, spotify_params};
use crate::queue_source::{self, Staged};
use crate::{CoreError, HouseholdState};
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;

/// At most ten actual tracks are resolved by an artist catalog read.
pub const MAX_SPOTIFY_TRACK_BATCH: usize = 10;

fn validate(tracks: &[(&str, &str)]) -> Result<(), CoreError> {
    if tracks.is_empty() || tracks.len() > MAX_SPOTIFY_TRACK_BATCH {
        return Err(ProtoError::Malformed(
            "a Spotify track batch must contain between 1 and 10 tracks".into(),
        )
        .into());
    }
    if tracks.iter().any(|(uri, _)| {
        !uri.strip_prefix("spotify:track:")
            .is_some_and(|id| id.len() == 22 && id.bytes().all(|byte| byte.is_ascii_alphanumeric()))
    }) {
        return Err(ProtoError::Malformed(
            "every batch item must be a canonical Spotify track with a 22-character id".into(),
        )
        .into());
    }
    Ok(())
}

fn render<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
) -> Result<Vec<(String, String)>, CoreError> {
    let params = spotify_params(t, households, coordinator)?.ok_or(CoreError::NoSpotifyFavorite)?;
    Ok(tracks
        .iter()
        .map(|(uri, title)| {
            (
                spotify_queue_uri(uri),
                spotify_track_didl(uri, title, &params),
            )
        })
        .collect())
}

fn requests<'a>(
    tracks: &[(&'a str, &str)],
    rendered: &'a [(String, String)],
    next: bool,
) -> Vec<queue_insert::Request<'a>> {
    tracks
        .iter()
        .zip(rendered)
        .map(|((source_uri, _), (uri, metadata))| queue_insert::Request {
            source_uri,
            uri,
            metadata,
            next,
        })
        .collect()
}

/// Append a known list of Spotify tracks, or insert it immediately after the
/// current queue occurrence. Tracks retain their input order; duplicate URIs
/// are separate occurrences. The operation never starts or pauses playback.
///
/// Only one household renderer template is learned. Each add is sent once and
/// checked against the complete queue and unchanged source/group/boot. A partial
/// failure may leave added tracks in place and returns an uncertain outcome;
/// callers must not replay it. The returned range supports existing queue undo.
pub fn enqueue_spotify_tracks<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
    next: bool,
) -> Result<Queued, CoreError> {
    validate(tracks)?;
    let host = addr(households, coordinator)?;
    let destination = SourceDestination::read(t, host, coordinator)?;
    enqueue_spotify_tracks_bound(t, households, coordinator, tracks, next, &destination)
}

/// Insert a resolved track list only while the destination observed before
/// catalog lookup still holds. Never adopt a newer group or selected source
/// between the surface's admission check and the first core read.
pub fn enqueue_spotify_tracks_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
    next: bool,
    destination: &SourceDestination,
) -> Result<Queued, CoreError> {
    validate(tracks)?;
    let host = addr(households, coordinator)?;
    destination.confirm(t, host, coordinator)?;
    let rendered = render(t, households, coordinator, tracks)?;
    let requests = requests(tracks, &rendered, next);
    Ok(queue_insert::enqueue_batch(t, host, coordinator, destination, &requests, next)?.queued)
}

/// Replace the group's queue with this exact track list and play from its
/// first occurrence in NORMAL order. The original queue is retained until the
/// entire appended batch is verified; removal uses its native queue revision.
/// A partial append or failed start is never retried automatically.
pub fn play_spotify_tracks<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
) -> Result<(), CoreError> {
    validate(tracks)?;
    let host = addr(households, coordinator)?;
    let destination = SourceDestination::read(t, host, coordinator)?;
    play_spotify_tracks_bound(t, households, coordinator, tracks, &destination)
}

/// Replace and play while retaining the pre-catalog destination through
/// renderer learning, every insertion, guarded removal and source selection.
pub fn play_spotify_tracks_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
    destination: &SourceDestination,
) -> Result<(), CoreError> {
    replace(t, households, coordinator, tracks, true, destination)
}

/// Stage and select the exact track list without Play. A playing old source is
/// paused first, so failure may leave it paused with some tracks appended.
pub fn load_spotify_tracks<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
) -> Result<(), CoreError> {
    validate(tracks)?;
    let host = addr(households, coordinator)?;
    let destination = SourceDestination::read(t, host, coordinator)?;
    load_spotify_tracks_bound(t, households, coordinator, tracks, &destination)
}

/// Silently load while retaining the original pre-catalog destination.
pub fn load_spotify_tracks_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
    destination: &SourceDestination,
) -> Result<(), CoreError> {
    replace(t, households, coordinator, tracks, false, destination)
}

fn replace<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[(&str, &str)],
    autoplay: bool,
    destination: &SourceDestination,
) -> Result<(), CoreError> {
    validate(tracks)?;
    let host = addr(households, coordinator)?;
    destination.confirm(t, host, coordinator)?;
    let rendered = render(t, households, coordinator, tracks)?;
    destination.confirm(t, host, coordinator)?;
    if !autoplay {
        pause_confirmed(t, households, coordinator)?;
        destination.confirm(t, host, coordinator)?;
    }
    let requests = requests(tracks, &rendered, false);
    let batch = queue_insert::enqueue_batch(t, host, coordinator, destination, &requests, false)?;
    queue_source::commit(
        t,
        households,
        coordinator,
        Staged { destination, before: &batch.before, after: &batch.after },
        autoplay,
        true,
    ).map_err(|error| CoreError::Uncertain(format!(
        "Spotify track replacement was not completed: {error}; the batch was not replayed; inspect the queue before retrying"
    )))
}
