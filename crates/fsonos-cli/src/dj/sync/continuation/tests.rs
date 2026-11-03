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
        playlists: pending(0, 105),
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
