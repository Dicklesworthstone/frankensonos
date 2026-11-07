//! Compact saved-library progress lives beside admission, never page bodies.

use super::{
    Checkpoint, LibrarySyncDto, Path, Running, STAMP, SyncedDto, followed_status, genre_status,
};
use fsonos_api::surface::dj_sync::{SavedLibraryIntakeDto, SavedLibraryIntakePhase};
use fsonos_spotify::cache::SavedLibraryPhase;

fn intake_status(saved: &Checkpoint, running: bool, now: i64) -> Option<SavedLibraryIntakeDto> {
    if !saved.intake_pending && !running {
        return None;
    }
    let progress = saved.intake.as_ref();
    let phase = match progress.map(|progress| progress.phase) {
        None | Some(SavedLibraryPhase::Reading) => SavedLibraryIntakePhase::Reading,
        Some(SavedLibraryPhase::Verifying) => SavedLibraryIntakePhase::Verifying,
        Some(SavedLibraryPhase::Ready) => SavedLibraryIntakePhase::Ready,
    };
    Some(SavedLibraryIntakeDto {
        pending: true,
        pages: progress.map_or(0, |progress| progress.pages),
        tracks: progress.map_or(0, |progress| progress.tracks),
        verified_pages: progress.map_or(0, |progress| progress.verified_pages),
        phase,
        running,
        retry_at: Some(saved.retry_at),
        not_before: (saved.not_before > now).then_some(saved.not_before),
    })
}

/// Observe one atomic stamp, so the accepted library, pending work and error
/// belong to the same checkpoint. No database, authentication or network read
/// is needed, even for an intake whose private page data is many megabytes.
pub(in crate::dj::sync) fn status(data_dir: &Path, running: &Running, now: i64) -> LibrarySyncDto {
    let value = std::fs::read_to_string(data_dir.join(STAMP))
        .ok()
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok());
    let last = value
        .as_ref()
        .and_then(|value| serde_json::from_value::<SyncedDto>(value.clone()).ok());
    let saved = value
        .as_ref()
        .and_then(|value| value.get("playlist_continuation"))
        .and_then(|value| serde_json::from_value::<Checkpoint>(value.clone()).ok())
        .unwrap_or_default();
    let error = running.error.clone().or_else(|| {
        (!running.running && saved.intake_pending)
            .then(|| saved.intake_error.clone())
            .flatten()
    });
    LibrarySyncDto::new(running.running, last, error)
        .with_genre_enrichment(genre_status(&saved, running.genres, now))
        .with_library_intake(intake_status(&saved, running.intake, now))
        .with_followed_artists(followed_status(&saved, running.followed, now))
}

#[cfg(test)]
mod tests;
