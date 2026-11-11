//! Real checkpoint files keep bounded intake observable without publishing it.

use super::*;
use crate::dj::sync::continuation::{PassKind, load, write};
use crate::dj::sync::{LibrarySync, ReadResult};
use fsonos_core::clock::FakeClock;
use fsonos_spotify::SpotifyError;
use fsonos_spotify::cache::{
    AlbumContinuation, GenreContinuation, LibrarySync as Counts, LibrarySyncDetailed,
    SavedLibraryProgress,
};
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use fsonos_spotify::taste::PlaylistContinuation;
use std::path::PathBuf;

const AT: i64 = 1_790_000_000;

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fsonos-library-intake-{}-{name}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn controller(path: PathBuf, at: i64) -> LibrarySync {
    LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        path,
    )
    .unwrap()
    .with_clock(Box::new(FakeClock::new(
        chrono::DateTime::from_timestamp(at, 0)
            .unwrap()
            .fixed_offset(),
    )))
}

fn progress(phase: SavedLibraryPhase) -> SavedLibraryProgress {
    SavedLibraryProgress {
        owner: "fixture-intake-owner".into(),
        base_owner: Some("fixture-public-owner".into()),
        base_generation: Some(3),
        pages: 512,
        tracks: 20_000,
        verified_pages: if phase == SavedLibraryPhase::Verifying {
            80
        } else {
            0
        },
        phase,
    }
}

fn previous() -> SyncedDto {
    SyncedDto {
        at: AT - 100,
        tracks: 20,
        candidates: 18,
        classical: 2,
        retired: 1,
        genre_artists_pending: 0,
        genre_stalled: 0,
        notes: Vec::new(),
    }
}

fn pending(report: SavedLibraryProgress) -> ReadResult {
    ReadResult {
        result: Err(SpotifyError::LibraryPending {
            pages: report.pages,
            tracks: report.tracks,
        }),
        retry_after: 0,
        progress: Some(report),
        progress_observed: true,
    }
}

#[test]
fn a_first_bounded_pass_remains_observable_after_restart_without_a_false_failure_or_success() {
    let dir = directory("first");
    let sync = controller(dir.clone(), AT);
    let checkpoint = Checkpoint::default();
    sync.finish_pass(
        None,
        &checkpoint,
        checkpoint.reserve_full(AT),
        PassKind::Full,
        Ok(pending(progress(SavedLibraryPhase::Reading))),
    )
    .unwrap();
    let saved = load(&dir).unwrap();
    assert!(saved.intake_pending && saved.requires_refresh);
    assert_eq!(saved.stalled, 0);
    let restarted = controller(dir.clone(), AT + 1);
    let status = restarted.status();
    assert!(status.last.is_none());
    assert!(status.error.is_none());
    assert!(!status.running);
    let intake = status.library_intake.as_ref().unwrap();
    assert!(intake.pending && !intake.running);
    assert_eq!((intake.pages, intake.tracks), (512, 20_000));
    assert_eq!(intake.phase, SavedLibraryIntakePhase::Reading);
    assert_eq!(intake.retry_at, Some(saved.retry_at));
    assert!(!restarted.due(saved.retry_at - 1));
    assert!(restarted.due(saved.retry_at));
    assert!(!restarted.take_fresh());
    let wire = serde_json::to_string(&status).unwrap();
    assert!(!wire.contains("fixture-intake-owner"));
    assert!(!wire.contains("fixture-public-owner"));
    assert!(!wire.contains("base_generation"));
    assert!(std::fs::metadata(dir.join(STAMP)).unwrap().len() < 4_096);
}

#[test]
fn interrupted_verification_retains_the_last_library_error_and_cooldown_across_restart() {
    let dir = directory("interrupted");
    let sync = controller(dir.clone(), AT);
    let checkpoint = Checkpoint::default();
    let outcome = ReadResult {
        result: Err(SpotifyError::Api {
            status: 429,
            body: "busy".into(),
        }),
        retry_after: 3_600,
        progress: Some(progress(SavedLibraryPhase::Verifying)),
        progress_observed: true,
    };
    assert!(
        sync.finish_pass(
            Some(previous()),
            &checkpoint,
            checkpoint.reserve_full(AT),
            PassKind::Full,
            Ok(outcome)
        )
        .is_err()
    );
    let saved = load(&dir).unwrap();
    let restarted = controller(dir.clone(), AT + 1);
    let status = restarted.status();
    assert_eq!(status.last, Some(previous()));
    assert!(status.error.as_deref().unwrap().contains("rate-limiting"));
    let intake = status.library_intake.unwrap();
    assert_eq!(intake.phase, SavedLibraryIntakePhase::Verifying);
    assert_eq!(intake.verified_pages, 80);
    assert_eq!(intake.not_before, Some(AT + 3_600));
    assert!(!restarted.due(AT + 3_599));
    assert!(saved.background_pass(AT + 3_599).is_none());
    assert!(
        restarted.run().is_err(),
        "explicit reads cannot bypass Spotify's cooldown"
    );
    assert!(!restarted.take_fresh());

    let resumed = controller(dir, AT + 3_600);
    let mut report = progress(SavedLibraryPhase::Verifying);
    report.verified_pages = 160;
    resumed
        .finish_pass(
            Some(previous()),
            &saved,
            saved.reserve_full(AT + 3_600),
            PassKind::Full,
            Ok(pending(report)),
        )
        .unwrap();
    assert!(resumed.status().error.is_none());
    assert_eq!(resumed.status().library_intake.unwrap().verified_pages, 160);
    assert_eq!(resumed.last(), Some(previous()));
}

#[test]
fn prepared_rows_remain_pending_until_full_publication_then_progress_is_cleared() {
    let dir = directory("publication");
    let sync = controller(dir.clone(), AT);
    let checkpoint = Checkpoint::default();
    let outcome = ReadResult {
        result: Err(SpotifyError::Store(
            fsonos_core::store::StoreError::Backend("publish failed".into()),
        )),
        retry_after: 0,
        progress: Some(progress(SavedLibraryPhase::Ready)),
        progress_observed: true,
    };
    assert!(
        sync.finish_pass(
            Some(previous()),
            &checkpoint,
            checkpoint.reserve_full(AT),
            PassKind::Full,
            Ok(outcome)
        )
        .is_err()
    );
    let status = sync.status();
    let intake = status.library_intake.unwrap();
    assert!(intake.pending);
    assert_eq!(intake.phase, SavedLibraryIntakePhase::Ready);
    assert_eq!(status.last, Some(previous()));
    assert!(!sync.take_fresh());
    let saved = load(&dir).unwrap();
    let read = LibrarySyncDetailed {
        library: Counts {
            tracks: 21,
            candidates: 19,
            classical: 3,
            retired: 1,
        },
        genres: GenreRead::default(),
        genre_continuation: GenreContinuation::default(),
        followed_artists: fsonos_spotify::taste::FollowedArtistContinuation::default(),
        artist_samples: fsonos_spotify::cache::ArtistContinuation::default(),
        playlists: PlaylistContinuation::default(),
        albums: AlbumContinuation::default(),
        notes: Vec::new(),
    };
    sync.finish_pass(
        Some(previous()),
        &saved,
        saved.reserve_full(AT),
        PassKind::Full,
        Ok(ReadResult {
            result: Ok(read),
            retry_after: 0,
            progress: None,
            progress_observed: true,
        }),
    )
    .unwrap();
    let status = sync.status();
    assert!(status.library_intake.is_none());
    assert!(status.error.is_none());
    assert_eq!(status.last.unwrap().at, AT);
    assert!(sync.take_fresh());
    let complete = load(&dir).unwrap();
    assert!(!complete.intake_pending);
    assert!(complete.intake.is_none() && complete.intake_error.is_none());
}

#[test]
fn admission_survives_a_crash_before_any_page_and_owner_counts_are_replaced_whole() {
    let dir = directory("admission");
    let checkpoint = Checkpoint::default();
    write(&dir, None, &checkpoint.reserve_full(AT)).unwrap();
    let sync = controller(dir.clone(), AT + 1);
    let status = sync.status();
    let intake = status.library_intake.unwrap();
    assert!(intake.pending);
    assert_eq!(
        (intake.pages, intake.tracks, intake.verified_pages),
        (0, 0, 0)
    );
    assert!(status.last.is_none() && status.error.is_none());

    let mut saved = load(&dir).unwrap();
    saved.intake = Some(progress(SavedLibraryPhase::Verifying));
    let next = SavedLibraryProgress {
        owner: "fixture-new-owner".into(),
        pages: 2,
        tracks: 40,
        verified_pages: 0,
        phase: SavedLibraryPhase::Reading,
        ..progress(SavedLibraryPhase::Reading)
    };
    sync.finish_pass(
        None,
        &saved,
        saved.reserve_full(AT),
        PassKind::Full,
        Ok(pending(next.clone())),
    )
    .unwrap();
    assert_eq!(load(&dir).unwrap().intake, Some(next));
    let status = sync.status().library_intake.unwrap();
    assert_eq!(
        (status.pages, status.tracks, status.verified_pages),
        (2, 40, 0)
    );
}

#[test]
fn worker_activity_is_specific_and_old_stamps_remain_readable() {
    let dir = directory("activity");
    std::fs::write(dir.join(STAMP), serde_json::to_vec(&previous()).unwrap()).unwrap();
    assert!(
        status(&dir, &Running::default(), AT)
            .library_intake
            .is_none()
    );
    let reading = Running {
        running: true,
        intake: true,
        ..Default::default()
    };
    let current = status(&dir, &reading, AT);
    assert!(current.running && current.library_intake.unwrap().running);
    let enriching = Running {
        running: true,
        genres: true,
        ..Default::default()
    };
    assert!(status(&dir, &enriching, AT).library_intake.is_none());
}

#[test]
fn a_losing_read_clears_its_old_counts_without_claiming_publication() {
    let dir = directory("changed-receipt");
    let sync = controller(dir.clone(), AT);
    let mut checkpoint = Checkpoint::default().reserve_full(AT);
    checkpoint.intake = Some(progress(SavedLibraryPhase::Verifying));
    write(&dir, Some(&previous()), &checkpoint).unwrap();
    let outcome = ReadResult {
        result: Err(SpotifyError::Store(
            fsonos_core::store::StoreError::Backend(
                "the account or saved-library read changed".into(),
            ),
        )),
        retry_after: 0,
        progress: None,
        progress_observed: true,
    };
    assert!(
        sync.finish_pass(
            Some(previous()),
            &checkpoint,
            checkpoint.reserve_full(AT),
            PassKind::Full,
            Ok(outcome)
        )
        .is_err()
    );
    let saved = load(&dir).unwrap();
    assert!(saved.intake.is_none());
    assert!(saved.intake_pending && saved.requires_refresh);
    let status = controller(dir, AT + 1).status();
    assert_eq!(status.last, Some(previous()));
    assert!(status.error.is_some());
    assert_eq!(status.library_intake.unwrap().pages, 0);
    assert!(!sync.take_fresh());
}

#[test]
fn an_owner_read_failure_preserves_retained_progress_and_real_errors_across_restart() {
    for status_code in [503, 429] {
        for has_previous_library in [false, true] {
            let dir = directory("owner-read-outage");
            let sync = controller(dir.clone(), AT);
            let checkpoint = Checkpoint::default();
            let before = has_previous_library.then(previous);
            let retained = progress(SavedLibraryPhase::Verifying);
            sync.finish_pass(
                before.clone(),
                &checkpoint,
                checkpoint.reserve_full(AT),
                PassKind::Full,
                Ok(pending(retained.clone())),
            )
            .unwrap();
            let saved = load(&dir).unwrap();
            let attempted_at = saved.retry_at;
            let retry_after = if status_code == 429 { 3_600 } else { 0 };
            let resumed = controller(dir.clone(), attempted_at);
            let outcome = ReadResult {
                result: Err(SpotifyError::Api {
                    status: status_code,
                    body: "owner read unavailable".into(),
                }),
                retry_after,
                progress: None,
                progress_observed: false,
            };
            assert!(
                resumed
                    .finish_pass(
                        before.clone(),
                        &saved,
                        saved.reserve_full(attempted_at),
                        PassKind::Full,
                        Ok(outcome),
                    )
                    .is_err()
            );
            let durable = load(&dir).unwrap();
            assert_eq!(durable.intake, Some(retained));
            assert!(durable.intake_pending && durable.requires_refresh);
            let restarted = controller(dir, attempted_at + 1);
            let status = restarted.status();
            assert_eq!(status.last, before);
            assert!(status.error.is_some(), "the real outage stays visible");
            assert!(!status.running);
            let intake = status.library_intake.as_ref().unwrap();
            assert_eq!(
                (intake.pages, intake.tracks, intake.verified_pages),
                (512, 20_000, 80)
            );
            assert_eq!(intake.phase, SavedLibraryIntakePhase::Verifying);
            assert_eq!(intake.retry_at, Some(durable.retry_at));
            if status_code == 429 {
                assert_eq!(intake.not_before, Some(attempted_at + 3_600));
                assert!(!restarted.due(attempted_at + 3_599));
            }
            assert!(!restarted.take_fresh());
            assert!(
                !serde_json::to_string(&status)
                    .unwrap()
                    .contains("progress_observed")
            );
        }
    }
}
