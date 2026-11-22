//! Scheduler persistence and admission, without Spotify or speaker I/O.

use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::LibrarySync as Counts;
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use std::path::PathBuf;

fn config() -> SpotifyConfig {
    SpotifyConfig {
        client_id: "0123456789abcdef0123456789abcdef".into(),
        redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
    }
}

fn directory(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-playlist-scheduler-{}-{name}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn controller(dir: PathBuf, at: i64) -> LibrarySync {
    LibrarySync::new(config(), dir)
        .unwrap()
        .with_clock(Box::new(FakeClock::new(
            chrono::DateTime::from_timestamp(at, 0)
                .unwrap()
                .fixed_offset(),
        )))
}

fn full(at: i64) -> SyncedDto {
    SyncedDto {
        at,
        tracks: 150,
        candidates: 150,
        classical: 0,
        retired: 2,
        genre_artists_pending: 0,
        genre_stalled: 0,
        notes: Vec::new(),
    }
}

fn pending(count: usize, advanced: u32) -> PlaylistContinuation {
    PlaylistContinuation {
        owner: Some("test-owner".into()),
        pending: count,
        advanced,
        ..PlaylistContinuation::default()
    }
}

#[test]
fn readiness_and_server_cooldown_survive_restart_and_cover_manual_full_reads() {
    let dir = directory("cooldown");
    let at = 1_790_000_000;
    let report = PlaylistContinuation {
        retry_after_secs: 3_600,
        ..pending(1, 150)
    };
    let checkpoint = Checkpoint::default().observed(&report, at);
    write(&dir, Some(&full(at)), &checkpoint).unwrap();

    let restarted = controller(dir.clone(), at + 3_599);
    let saved = load(&dir).unwrap();
    assert_eq!(saved, checkpoint);
    assert_eq!(restarted.last(), Some(full(at)));
    assert!(saved.owner_due(at + 3_599).is_none());
    assert!(!restarted.due(at + 3_599));
    let failure = restarted.run().unwrap_err();
    assert_eq!(failure.code, ErrorCode::NotReady);
    assert!(failure.detail.contains("no refresh request was sent"));
    assert!(!restarted.take_fresh());
    assert_eq!(load(&dir), Some(checkpoint));
    assert_eq!(saved.owner_due(at + 3_600).as_deref(), Some("test-owner"));
}

#[test]
fn successful_continuation_updates_the_pool_without_postponing_daily_full_refresh() {
    let dir = directory("daily");
    let at = 1_790_000_000;
    let day = i64::try_from(MAX_AGE.as_secs()).unwrap();
    let read = LibrarySyncDetailed {
        library: Counts {
            tracks: 405,
            candidates: 400,
            classical: 7,
            retired: 0,
        },
        genres: GenreRead::default(),
        genre_continuation: GenreContinuation::default(),
        followed_artists: fsonos_spotify::taste::FollowedArtistContinuation::default(),
        artist_samples: fsonos_spotify::cache::ArtistContinuation::default(),
        playlists: pending(0, 105),
        albums: AlbumContinuation::default(),
        notes: Vec::new(),
    };
    let latest = continued_result(full(at), &read, at + day - 60);
    assert_eq!(latest.at, at);
    assert_eq!(
        (latest.tracks, latest.candidates, latest.classical),
        (405, 400, 7)
    );
    assert_eq!(
        latest.retired, 2,
        "only a full sync can retire unrelated music"
    );
    let checkpoint = Checkpoint::default().observed(&read.playlists, at + day - 60);
    write(&dir, Some(&latest), &checkpoint).unwrap();
    let restarted = controller(dir, at + day);
    assert!(
        restarted.due(at + day),
        "continuation cannot defer the daily full read"
    );
    assert!(
        checkpoint.owner_due(at + day).is_none(),
        "completed intake is not rescanned"
    );
}

#[test]
fn failures_back_off_but_newly_accepted_positions_reset_the_wait() {
    let at = 1_790_000_000;
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    let mut checkpoint = Checkpoint::default().observed(&pending(2, 150), at);
    assert_eq!(checkpoint.retry_at, at + check);
    for failures in 1..=4 {
        let next = checkpoint.retry_at;
        checkpoint = checkpoint.observed(&pending(2, 0), next);
        assert_eq!(checkpoint.stalled, failures);
        assert!(checkpoint.owner_due(next + check - 1).is_none());
    }
    let next = checkpoint.retry_at;
    checkpoint = checkpoint.observed(&pending(2, 150), next);
    assert_eq!(
        checkpoint.stalled, 0,
        "progress inside a long list counts even before completion"
    );
    assert_eq!(checkpoint.retry_at, next + check);
    checkpoint = checkpoint.observed(
        &PlaylistContinuation {
            limited: 2,
            ..pending(0, 0)
        },
        next + check,
    );
    assert!(
        checkpoint.owner_due(i64::MAX).is_none(),
        "cache limits are terminal"
    );
}

#[test]
fn admission_is_persisted_before_a_crash_and_cannot_create_a_false_full_sync() {
    let dir = directory("interrupted");
    let at = 1_790_000_000;
    let checkpoint = Checkpoint::default().observed(&pending(3, 150), at);
    let admitted = checkpoint.reserve(checkpoint.retry_at);
    write(&dir, None, &admitted).unwrap();
    let restarted = controller(dir.clone(), checkpoint.retry_at);
    assert_eq!(
        restarted.last(),
        None,
        "private scheduling state is not a completed library read"
    );
    assert_eq!(load(&dir), Some(admitted.clone()));
    assert!(admitted.owner_due(checkpoint.retry_at).is_none());
    assert!(admitted.retry_at > checkpoint.retry_at);
}

#[test]
fn old_stamps_remain_readable_and_an_account_change_requires_full_refresh() {
    let dir = directory("compatibility");
    let at = 1_790_000_000;
    super::super::write_stamp(&dir, &full(at)).unwrap();
    let sync = controller(dir.clone(), at);
    assert_eq!(sync.last(), Some(full(at)));
    assert!(
        load(&dir).is_none(),
        "old stamps do not invent a continuation owner"
    );
    let checkpoint = Checkpoint::default().observed(
        &PlaylistContinuation {
            owner: Some("new-owner".into()),
            requires_refresh: true,
            ..PlaylistContinuation::default()
        },
        at,
    );
    write(&dir, Some(&full(at)), &checkpoint).unwrap();
    assert!(checkpoint.owner_due(i64::MAX).is_none());
    assert!(!sync.due(at));
    assert!(sync.due(checkpoint.retry_at));
}

#[test]
fn a_background_check_releases_idle_admission_and_preserves_an_existing_worker() {
    let dir = directory("admission");
    let at = 1_790_000_000;
    let checkpoint = Checkpoint::default().observed(&pending(1, 150), at);
    write(&dir, Some(&full(at)), &checkpoint).unwrap();
    let sync = controller(dir, at);
    sync.running().error = Some("the previous pass failed".into());
    assert!(sync.background().is_none(), "no Spotify work is due yet");
    assert!(!sync.running().running);
    assert_eq!(
        sync.running().error.as_deref(),
        Some("the previous pass failed")
    );
    assert!(!sync.take_fresh());

    assert!(sync.claim(), "idle background admission was released");
    assert!(
        sync.background().is_none(),
        "an admitted manual refresh owns the slot"
    );
    assert!(
        sync.running().running,
        "background must not release another worker"
    );
    sync.running().running = false;
}

#[test]
fn completed_intake_requests_one_durable_full_reconciliation_after_server_cooldown() {
    let dir = directory("reconcile");
    let at = 1_790_000_000;
    let waiting = Checkpoint::default().observed(&pending(3, 150), at);
    let finished_at = waiting.retry_at;
    let complete = PlaylistContinuation {
        retry_after_secs: 3_600,
        ..pending(0, 105)
    };
    let next = waiting.completed(&complete, finished_at, true);
    assert!(next.reconcile);
    assert!(!next.requires_refresh, "this is not an account change");
    assert!(next.note().contains("reconcile the library"));
    assert!(next.owner_due(i64::MAX).is_none());
    write(&dir, Some(&full(at)), &next).unwrap();

    let restarted = controller(dir.clone(), finished_at + 3_599);
    assert_eq!(load(&dir), Some(next.clone()));
    assert_eq!(restarted.last().unwrap().at, at);
    assert!(!restarted.due(finished_at + 3_599));
    assert!(restarted.due(finished_at + 3_600));

    // A successful full read consumes the intent without postponing the
    // original daily deadline on any of the preceding continuation passes.
    let accepted = next.completed(&pending(0, 0), finished_at + 3_600, false);
    assert!(!accepted.reconcile);
    assert!(accepted.owner_due(i64::MAX).is_none());
    write(&dir, Some(&full(finished_at + 3_600)), &accepted).unwrap();
    let restarted = controller(dir, finished_at + 3_600);
    assert!(!restarted.due(finished_at + 3_600));
    let day = i64::try_from(MAX_AGE.as_secs()).unwrap();
    assert!(restarted.due(finished_at + 3_600 + day));
}

#[test]
fn failed_full_reconciliation_keeps_its_intent_and_last_success_after_restart() {
    let dir = directory("reconcile-failure");
    let at = 1_790_000_000;
    let waiting = Checkpoint::default().observed(&pending(1, 150), at);
    let complete = waiting.completed(&pending(0, 50), waiting.retry_at, true);
    assert!(complete.reconcile);
    write(&dir, Some(&full(at)), &complete).unwrap();

    // No token is present, so the real full-refresh path fails before HTTP.
    // Its admission and error persistence must keep the reconciliation due.
    let sync = controller(dir.clone(), complete.retry_at);
    assert!(sync.due(sync.now()));
    assert_eq!(sync.run().unwrap_err().code, ErrorCode::SpotifyAuthRequired);
    let saved = load(&dir).unwrap();
    assert!(saved.reconcile);
    assert_eq!(sync.last(), Some(full(at)));
    assert!(!sync.status().running);
    assert!(!sync.take_fresh());
    assert!(saved.retry_at > complete.retry_at);
    let restarted = controller(dir, saved.retry_at);
    assert!(restarted.due(saved.retry_at));
    assert!(saved.owner_due(i64::MAX).is_none());
}

#[test]
fn incomplete_limited_and_changed_owner_passes_cannot_claim_reconciliation() {
    let at = 1_790_000_000;
    let waiting = Checkpoint::default().observed(&pending(3, 150), at);
    let incomplete = pending(1, 150);
    let limited = PlaylistContinuation {
        limited: 1,
        ..pending(0, 150)
    };
    let changed = PlaylistContinuation {
        owner: Some("different-owner".into()),
        requires_refresh: true,
        ..pending(0, 150)
    };
    let unknown = PlaylistContinuation {
        owner: None,
        ..pending(0, 0)
    };
    for report in [incomplete, limited, changed, unknown] {
        assert!(!waiting.completed(&report, at + 900, true).reconcile);
    }
    assert!(
        !Checkpoint::default()
            .completed(&pending(0, 0), at, true)
            .reconcile
    );
    assert!(!waiting.completed(&pending(0, 0), at, false).reconcile);

    // Stamps from before this field existed still retain their owner and
    // cooldown without inventing a pending full reconciliation.
    let mut legacy = serde_json::to_value(&waiting).unwrap();
    legacy.as_object_mut().unwrap().remove("reconcile");
    let decoded: Checkpoint = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded, waiting);
}

#[test]
fn a_failed_manual_full_read_remains_due_after_a_recent_success() {
    let dir = directory("manual-full-retry");
    let at = 1_790_000_000;
    write(&dir, Some(&full(at)), &Checkpoint::default()).unwrap();
    let sync = controller(dir.clone(), at + 10);
    assert!(!sync.due(sync.now()));

    // The real full-read admission path fails before HTTP because no token
    // exists. The old success must not make this request disappear.
    assert_eq!(sync.run().unwrap_err().code, ErrorCode::SpotifyAuthRequired);
    let saved = load(&dir).unwrap();
    assert!(saved.requires_refresh);
    assert_eq!(sync.last(), Some(full(at)));
    assert!(!sync.take_fresh());
    assert!(saved.retry_at > sync.now());

    let restarted = controller(dir, saved.retry_at);
    assert!(!restarted.due(saved.retry_at - 1));
    assert!(restarted.due(saved.retry_at));
    assert!(saved.owner_due(i64::MAX).is_none());
    assert!(!saved.album_due(i64::MAX));
    assert!(!saved.genre_due(i64::MAX));
}

#[test]
fn an_overdue_success_cannot_bypass_a_reserved_full_read_deadline() {
    let dir = directory("overdue-full-deadline");
    let at = 1_790_000_000;
    let day = i64::try_from(MAX_AGE.as_secs()).unwrap();
    let now = at + day + 10;
    let mut reserved = Checkpoint::default().reserve_full(now);
    reserved.cooldown(now, 7_200);
    write(&dir, Some(&full(at)), &reserved).unwrap();

    let restarted = controller(dir, now);
    assert!(reserved.requires_refresh);
    assert!(!restarted.due(now));
    assert!(!restarted.due(reserved.retry_at - 1));
    assert!(restarted.due(reserved.retry_at));
    assert_eq!(restarted.last(), Some(full(at)));
}

#[test]
fn healthy_saved_library_progress_continues_without_a_stall_penalty() {
    let dir = directory("saved-library-progress");
    let at = 1_790_000_000;
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    let previous = Checkpoint {
        stalled: 8,
        ..Checkpoint::default().observed(&pending(3, 0), at)
    };
    let mut reserved = previous.reserve_full(at);
    assert!(reserved.retry_at > at + check);

    reserved.continue_full(at + 10);
    assert!(reserved.requires_refresh);
    assert_eq!(reserved.stalled, 0);
    assert_eq!(reserved.retry_at, at + 10 + check);
    assert_eq!(reserved.owner, previous.owner);
    assert_eq!(reserved.pending, previous.pending);
    assert!(reserved.owner_due(i64::MAX).is_none());
    assert!(!reserved.album_due(i64::MAX));
    assert!(!reserved.genre_due(i64::MAX));

    // A server-requested cooldown still wins over the next normal check.
    reserved.cooldown(at + 10, 3_600);
    assert_eq!(reserved.retry_at, at + 3_610);
    write(&dir, Some(&full(at)), &reserved).unwrap();
    let restarted = controller(dir, at + 20);
    assert_eq!(load(&restarted.data_dir), Some(reserved.clone()));
    assert!(!restarted.due(reserved.retry_at - 1));
    assert!(restarted.due(reserved.retry_at));
    assert_eq!(restarted.last(), Some(full(at)));
    assert!(!restarted.take_fresh());
}

#[test]
fn an_accepted_full_read_consumes_its_durable_refresh_intent() {
    let dir = directory("accepted-full-intent");
    let at = 1_790_000_000;
    let checkpoint = Checkpoint::default().reserve_full(at);
    write(&dir, Some(&full(at)), &checkpoint).unwrap();
    let read = LibrarySyncDetailed {
        library: Counts {
            tracks: 200,
            candidates: 195,
            classical: 5,
            retired: 2,
        },
        genres: GenreRead::default(),
        genre_continuation: GenreContinuation::default(),
        followed_artists: fsonos_spotify::taste::FollowedArtistContinuation::default(),
        artist_samples: fsonos_spotify::cache::ArtistContinuation::default(),
        playlists: pending(0, 100),
        albums: AlbumContinuation::default(),
        notes: Vec::new(),
    };
    let sync = controller(dir.clone(), at + 60);
    let accepted = sync
        .accepted(Some(full(at)), &checkpoint, &read, PassKind::Full, at + 60)
        .unwrap();
    assert_eq!(accepted.at, at + 60);
    assert_eq!(accepted.tracks, 200);
    assert!(sync.take_fresh());
    let saved = load(&dir).unwrap();
    assert!(!saved.requires_refresh);
    assert!(!saved.reconcile);
    assert!(!sync.due(at + 61));
}

#[test]
fn a_saved_library_window_reports_progress_without_claiming_success() {
    let error = failure(fsonos_spotify::SpotifyError::LibraryPending {
        pages: 512,
        tracks: 25_600,
    });
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(error.detail.contains("512 page(s)"));
    assert!(error.detail.contains("25600 track(s)"));
    assert!(error.detail.contains("public library is unchanged"));
    assert!(error.detail.contains("next refresh continues"));
}

#[test]
fn another_directory_writer_blocks_manual_and_background_reads_without_changing_progress() {
    let dir = directory("writer-contention");
    let at = 1_790_000_000;
    let checkpoint = Checkpoint::default().observed(&pending(3, 100), at);
    write(&dir, Some(&full(at)), &checkpoint).unwrap();
    let original = std::fs::read(dir.join(STAMP)).unwrap();
    let writer = controller(dir.clone(), at);
    let lock = writer.try_claim_directory().unwrap().unwrap();
    let contender = controller(dir.clone(), at + 100_000);

    let error = contender.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(error.detail.contains("another process"));
    assert!(!contender.running().running, "release the local slot");
    assert_eq!(std::fs::read(dir.join(STAMP)).unwrap(), original);

    contender.running().error = Some("previous reader failed".into());
    assert!(
        contender.background().is_none(),
        "a busy writer is idle work"
    );
    assert!(!contender.running().running);
    assert_eq!(
        contender.running().error.as_deref(),
        Some("previous reader failed")
    );
    assert_eq!(std::fs::read(dir.join(STAMP)).unwrap(), original);
    assert!(!contender.take_fresh());

    drop(lock);
    let error = contender.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::SpotifyAuthRequired);
    assert!(!contender.running().running);
    assert!(
        writer.try_claim_directory().unwrap().is_some(),
        "a failed read must release the directory lock"
    );
}

#[test]
fn a_retried_reader_observes_the_latest_writer_cooldown_before_admission() {
    let dir = directory("writer-cooldown");
    let at = 1_790_000_000;
    let reader = controller(dir.clone(), at);
    let writer = controller(dir.clone(), at);
    let lock = writer.try_claim_directory().unwrap().unwrap();
    assert_eq!(reader.run().unwrap_err().code, ErrorCode::NotReady);

    let mut saved = Checkpoint::default().observed(&pending(2, 50), at);
    saved.cooldown(at, 3_600);
    write(&dir, Some(&full(at)), &saved).unwrap();
    let original = std::fs::read(dir.join(STAMP)).unwrap();
    drop(lock);

    let error = reader.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(error.detail.contains("no refresh request was sent"));
    assert!(reader.background().is_none());
    assert_eq!(std::fs::read(dir.join(STAMP)).unwrap(), original);
    assert!(!reader.running().running);
}

#[test]
fn refresh_locks_are_per_directory_and_failed_admission_releases_the_local_slot() {
    let at = 1_790_000_000;
    let first = controller(directory("writer-first"), at);
    let second = controller(directory("writer-second"), at);
    let _first = first.try_claim_directory().unwrap().unwrap();
    let _second = second.try_claim_directory().unwrap().unwrap();

    let dir = directory("writer-open-error");
    let blocked = dir.join(super::super::REFRESH_LOCK);
    std::fs::create_dir(&blocked).unwrap();
    let reader = controller(dir.clone(), at);
    assert_eq!(reader.run().unwrap_err().code, ErrorCode::Internal);
    assert!(!reader.running().running);
    assert_eq!(
        reader.background().unwrap().unwrap_err().code,
        ErrorCode::Internal
    );
    assert!(!reader.running().running);
    assert!(!dir.join(STAMP).exists());

    std::fs::rename(blocked, dir.join("former-lock-directory")).unwrap();
    let lock = reader.try_claim_directory().unwrap().unwrap();
    assert!(
        reader.claim(),
        "failed admission must allow another attempt"
    );
    reader.running().running = false;
    drop(lock);
    assert!(reader.try_claim_directory().unwrap().is_some());
}

#[test]
fn checkpoint_publication_uses_private_staging_and_leaves_another_writers_file_intact() {
    let dir = directory("writer-publication");
    let at = 1_790_000_000;
    let old_temporary = dir.join(format!("{STAMP}.tmp"));
    std::fs::write(&old_temporary, "another writer's incomplete checkpoint").unwrap();
    let checkpoint = Checkpoint::default().observed(&pending(3, 100), at);
    write(&dir, Some(&full(at)), &checkpoint).unwrap();
    assert_eq!(
        std::fs::read_to_string(old_temporary).unwrap(),
        "another writer's incomplete checkpoint"
    );
    assert_eq!(load(&dir), Some(checkpoint));
    assert_eq!(super::super::last_synced(&dir), Some(full(at)));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(dir.join(STAMP))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
