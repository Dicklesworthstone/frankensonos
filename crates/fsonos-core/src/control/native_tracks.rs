//! Concrete native playlist tracks retain their complete speaker-owned DIDL.

use super::{
    MAX_TRACK_BATCH, QueueReplacement, Queued, TrackBatchDestination, addr, queue_insert,
    track_batch,
};
use crate::favorites::Favorite;
use crate::{CoreError, HouseholdState, playlists};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;

fn requests(tracks: &[Favorite], next: bool) -> Result<Vec<queue_insert::Request<'_>>, CoreError> {
    if tracks.is_empty() || tracks.len() > MAX_TRACK_BATCH {
        return Err(ProtoError::Malformed(
            "a native track batch must contain between 1 and 10 tracks".into(),
        )
        .into());
    }
    tracks
        .iter()
        .map(|track| {
            playlists::validate_native_track(track)?;
            let uri = track
                .uri
                .as_deref()
                .expect("validated native tracks have a resource");
            Ok(queue_insert::Request {
                source_uri: uri,
                uri,
                metadata: &track.metadata,
                next,
            })
        })
        .collect()
}

/// Insert 1–10 previously selected native tracks next or at the end while
/// retaining the pre-selection destination. Caller order and repetitions are
/// preserved, including local files and non-Spotify service resources. All
/// tracks must retain their observed DIDL; containers and streams are refused
/// before I/O. No renderer lookup, Play, Pause or source change is performed.
///
/// Each add is sent once and verified. A partial or unconfirmed insertion
/// returns an uncertain outcome without replay; a completed range carries the
/// same exact-range undo proof as other concrete track selections.
pub fn enqueue_native_tracks_bound<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[Favorite],
    next: bool,
    destination: &TrackBatchDestination,
) -> Result<Queued, CoreError> {
    let requests = requests(tracks, next)?;
    let host = addr(households, coordinator)?;
    Ok(queue_insert::enqueue_batch(t, host, coordinator, destination, &requests, next)?.queued)
}

/// Replace the selected queue with native occurrences and start its first
/// track in NORMAL order. The entire new list is staged and verified before
/// the old prefix is removed. The original source, group, queue and boot stay
/// bound through preparation and every write. A failed start cannot replay
/// the replacement; a successful result supports the shared bounded undo.
pub fn play_native_tracks_bound_with_receipt<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[Favorite],
    destination: &TrackBatchDestination,
) -> Result<QueueReplacement, CoreError> {
    let requests = requests(tracks, false)?;
    track_batch::replace(
        t,
        households,
        coordinator,
        &requests,
        true,
        destination,
        true,
    )
}

/// Silently replace the selected queue with native occurrences in NORMAL
/// order. Silence is confirmed before insertion; no Play is sent. The saved
/// selection retains the same destination and exact replacement proof as
/// immediate native playback.
pub fn load_native_tracks_bound_with_receipt<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    tracks: &[Favorite],
    destination: &TrackBatchDestination,
) -> Result<QueueReplacement, CoreError> {
    let requests = requests(tracks, false)?;
    track_batch::replace(
        t,
        households,
        coordinator,
        &requests,
        false,
        destination,
        true,
    )
}
