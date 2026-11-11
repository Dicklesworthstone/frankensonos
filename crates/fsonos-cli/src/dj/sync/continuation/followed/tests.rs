//! Real scheduler stamps retain directory work without claiming a full sync.

use super::*;
use crate::dj::sync::continuation::{Pass, PassKind, load, write};
use crate::dj::sync::{CHECK, LibrarySync, LibrarySyncDetailed, MAX_AGE, SyncedDto};
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::{AlbumContinuation, GenreContinuation, LibrarySync as Counts};
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use fsonos_spotify::taste::PlaylistContinuation;
use serde_json::json;
use std::path::PathBuf;

const AT: i64 = 1_790_000_000;

fn directory(name: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "fsonos-followed-scheduler-{}-{name}-{}",
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

fn full() -> SyncedDto {
    SyncedDto {
        at: AT,
        tracks: 120,
        candidates: 118,
        classical: 2,
        retired: 3,
        genre_artists_pending: 0,
        genre_stalled: 0,
        notes: vec!["an unrelated diagnostic".into()],
    }
}

fn progress(
    pages: usize,
    verified: usize,
    phase: FollowedArtistPhase,
) -> FollowedArtistContinuation {
    FollowedArtistContinuation {
        owner: Some("test-owner".into()),
        generation: Some(1),
        pending: true,
        pages,
        artists: pages * 50,
        verified_pages: verified,
        phase,
        ..FollowedArtistContinuation::default()
    }
}

fn report(followed: FollowedArtistContinuation) -> LibrarySyncDetailed {
    LibrarySyncDetailed {
        library: Counts {
            tracks: 120,
            candidates: 118,
            classical: 2,
            retired: 0,
        },
        genres: GenreRead::default(),
        genre_continuation: serde_json::from_value(json!({
            "owner":"test-owner", "generation":1, "pending":0
        }))
        .unwrap(),
        playlists: PlaylistContinuation {
            owner: Some("test-owner".into()),
            ..Default::default()
        },
        followed_artists: followed,
        artist_samples: fsonos_spotify::cache::ArtistContinuation::default(),
        albums: serde_json::from_value(json!({
            "owner":"test-owner", "generation":1, "pending":0
        }))
        .unwrap(),
        notes: Vec::new(),
    }
}

fn checkpoint() -> Checkpoint {
    let read = report(progress(20, 0, FollowedArtistPhase::Reading));
    let mut state = Checkpoint::default().observed(&read.playlists, AT);
    state.observed_albums(&read.albums, AT, false);
    state.observed_genres(&read.genre_continuation, AT, false);
    state.observed_followed(&read.followed_artists, AT);
    state
}

#[test]
fn restart_resumes_directory_progress_without_postponing_daily_full_refresh() {
    let path = directory("restart");
    let checkpoint = checkpoint();
    let at = checkpoint.followed_retry_at;
    write(&path, Some(&full()), &checkpoint).unwrap();
    let sync = controller(path.clone(), at);
    assert!(!sync.due(at));
    assert!(matches!(
        checkpoint.background_pass(at),
        Some(Pass::Followed(_))
    ));
    let mut next = progress(39, 0, FollowedArtistPhase::Reading);
    next.advanced = 19;
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(next.clone()),
        PassKind::Followed,
        at,
    )
    .unwrap();
    assert_eq!(sync.last().unwrap().at, AT);
    assert_eq!(sync.last().unwrap().retired, 3);
    assert!(
        sync.last()
            .unwrap()
            .notes
            .iter()
            .any(|note| note == "an unrelated diagnostic")
    );
    let saved = load(&path).unwrap();
    assert_eq!(saved.followed.as_ref(), Some(&next));
    let restarted = controller(path, saved.followed_retry_at);
    let status = restarted.status().followed_artists.unwrap();
    assert_eq!((status.pages, status.artists), (39, 1_950));
    assert!(status.pending);
    assert_eq!(status.retry_at, Some(saved.followed_retry_at));
    assert!(!restarted.due(saved.followed_retry_at));
    assert!(restarted.due(AT + i64::try_from(MAX_AGE.as_secs()).unwrap()));
}

#[test]
fn verified_membership_waits_for_full_publication_and_intent_survives_restart() {
    let path = directory("reconcile");
    let checkpoint = checkpoint();
    let at = checkpoint.followed_retry_at;
    let sync = controller(path.clone(), at);
    let mut ready = progress(21, 21, FollowedArtistPhase::Ready);
    ready.advanced = 2;
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(ready.clone()),
        PassKind::Followed,
        at,
    )
    .unwrap();
    let saved = load(&path).unwrap();
    assert!(saved.reconcile);
    assert!(saved.background_pass(i64::MAX).is_none());
    assert_eq!(sync.last().unwrap().at, AT);
    let restarted = controller(path.clone(), saved.retry_at);
    assert!(restarted.due(saved.retry_at));
    assert!(restarted.status().followed_artists.unwrap().pending);
    // A full read that cannot acknowledge the directory keeps reconciliation.
    ready.advanced = 0;
    restarted
        .accepted(
            restarted.last(),
            &saved,
            &report(ready.clone()),
            PassKind::Full,
            saved.retry_at,
        )
        .unwrap();
    let still_pending = load(&path).unwrap();
    assert!(still_pending.reconcile);
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    assert_eq!(still_pending.retry_at, saved.retry_at + 2 * check);
    assert_eq!(still_pending.followed_stalled, 1);
    restarted
        .accepted(
            restarted.last(),
            &still_pending,
            &report(ready.clone()),
            PassKind::Full,
            still_pending.retry_at,
        )
        .unwrap();
    let still_pending = load(&path).unwrap();
    assert_eq!(still_pending.followed_stalled, 2);
    assert_eq!(still_pending.retry_at, still_pending.followed_retry_at);
    ready.pending = false;
    ready.advanced = 0;
    let finished_at = still_pending.retry_at;
    restarted
        .accepted(
            restarted.last(),
            &still_pending,
            &report(ready),
            PassKind::Full,
            finished_at,
        )
        .unwrap();
    let complete = load(&path).unwrap();
    assert!(!complete.reconcile);
    assert!(!complete.followed.unwrap().pending);
    assert_eq!(restarted.last().unwrap().at, finished_at);
    assert!(!restarted.due(finished_at));
}

#[test]
fn four_readers_rotate_and_unrelated_observations_preserve_followed_backoff() {
    let path = directory("rotation");
    let sync = controller(path.clone(), AT);
    let mut state = checkpoint();
    state.pending = 2;
    let albums: AlbumContinuation = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":1
    }))
    .unwrap();
    let genres: GenreContinuation = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":5
    }))
    .unwrap();
    state.albums = Some(albums.clone());
    state.genres = Some(genres.clone());
    state.followed_stalled = 3;
    let deadline = state.followed_retry_at;
    let retained = state.followed.clone();
    let mut kinds = Vec::new();
    for _ in 0..8 {
        let at = state
            .retry_at
            .max(state.album_retry_at)
            .max(state.genre_retry_at)
            .max(state.followed_retry_at);
        let kind = state.background_pass(at).unwrap().kind();
        kinds.push(kind);
        let mut read = report(state.followed.clone().unwrap());
        read.playlists.pending = 2;
        read.playlists.advanced = 1;
        read.albums = albums.clone();
        read.genre_continuation = genres.clone();
        match kind {
            PassKind::Albums => read.albums.advanced = 1,
            PassKind::Genres => read.genre_continuation.completed = 1,
            PassKind::Followed => {
                read.followed_artists.pages += 1;
                read.followed_artists.advanced = 1;
            }
            _ => {}
        }
        sync.accepted(Some(full()), &state, &read, kind, at)
            .unwrap();
        state = load(&path).unwrap();
        if kinds.len() <= 3 {
            assert_eq!(state.followed, retained);
            assert_eq!(state.followed_retry_at, deadline);
            assert_eq!(state.followed_stalled, 3);
        }
    }
    assert_eq!(
        kinds,
        [
            PassKind::Playlists,
            PassKind::Albums,
            PassKind::Genres,
            PassKind::Followed,
            PassKind::Playlists,
            PassKind::Albums,
            PassKind::Genres,
            PassKind::Followed
        ]
    );
}

#[test]
fn followed_rate_limits_and_admission_survive_restart_and_block_every_reader() {
    let path = directory("cooldown");
    let checkpoint = checkpoint();
    let at = checkpoint.followed_retry_at;
    let mut admitted = checkpoint.reserve_followed(at);
    assert_eq!(admitted.followed_stalled, 1);
    assert_eq!(admitted.retry_at, checkpoint.retry_at);
    admitted.cooldown(at, 3_600);
    write(&path, Some(&full()), &admitted).unwrap();
    let saved = load(&path).unwrap();
    assert_eq!(saved, admitted);
    assert!(saved.background_pass(at + 3_599).is_none());
    let sync = controller(path.clone(), at + 3_599);
    assert!(
        sync.run()
            .unwrap_err()
            .detail
            .contains("no refresh request was sent")
    );
    assert_eq!(load(&path).unwrap(), admitted);
    assert_eq!(
        sync.status().followed_artists.unwrap().not_before,
        Some(at + 3_600)
    );
    assert_eq!(sync.last().unwrap().at, AT);
}

#[test]
fn owner_changes_and_terminal_limits_never_claim_completed_membership() {
    let path = directory("owner");
    let checkpoint = checkpoint();
    let at = checkpoint.followed_retry_at;
    let sync = controller(path.clone(), at);
    let changed = FollowedArtistContinuation::refresh_required();
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(changed),
        PassKind::Followed,
        at,
    )
    .unwrap();
    let saved = load(&path).unwrap();
    assert!(saved.requires_refresh);
    assert!(saved.background_pass(i64::MAX).is_none());
    assert!(sync.due(saved.retry_at));
    assert!(!sync.take_fresh());
    let mut limited = progress(20, 0, FollowedArtistPhase::Reading);
    limited.pending = false;
    limited.limited = true;
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(limited),
        PassKind::Followed,
        at,
    )
    .unwrap();
    let saved = load(&path).unwrap();
    assert!(saved.background_pass(i64::MAX).is_none());
    assert!(!saved.reconcile);
    let status = sync.status().followed_artists.unwrap();
    assert!(status.limited);
    assert!(!status.pending);
    assert!(status.retry_at.is_none());
}

#[test]
fn missing_followed_fields_in_older_stamps_remain_readable() {
    let mut value = serde_json::to_value(checkpoint()).unwrap();
    for key in ["followed", "followed_stalled", "followed_retry_at"] {
        value.as_object_mut().unwrap().remove(key);
    }
    let legacy: Checkpoint = serde_json::from_value(value).unwrap();
    assert!(legacy.followed.is_none());
    assert!(
        legacy
            .followed_due(AT + i64::try_from(CHECK.as_secs()).unwrap())
            .is_none()
    );
    let mut state = checkpoint();
    state.observed_followed(&FollowedArtistContinuation::default(), AT);
    assert!(state.followed.is_none());
    assert!(followed_status(&state, false, AT).is_none());
}
