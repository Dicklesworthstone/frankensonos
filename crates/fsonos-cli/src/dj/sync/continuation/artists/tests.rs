//! Scheduler receipts survive restart and keep finite-cohort progress separate
//! from full publication, the other readers and Spotify's shared cooldown.

use super::*;
use crate::dj::sync::continuation::{Pass, PassKind, load, write};
use crate::dj::sync::{CHECK, LibrarySync, LibrarySyncDetailed, MAX_AGE, ReadResult, SyncedDto};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::{AlbumContinuation, GenreContinuation, LibrarySync as Counts};
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use fsonos_spotify::taste::{
    FollowedArtistContinuation, FollowedArtistPhase, PlaylistContinuation,
};
use serde_json::json;
use std::path::Path;

const AT: i64 = 1_790_000_000;

fn controller(path: &Path, at: i64) -> LibrarySync {
    LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        path.to_owned(),
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
        tracks: 150,
        candidates: 148,
        classical: 2,
        retired: 4,
        genre_artists_pending: 0,
        genre_stalled: 0,
        notes: vec!["an unrelated diagnostic".into()],
    }
}

fn samples(pending: usize, completed: usize) -> ArtistContinuation {
    ArtistContinuation {
        owner: Some("test-owner".into()),
        generation: Some(1),
        cycle: Some("sample-cycle-a".into()),
        pending,
        completed,
        attempted: 50,
        ..Default::default()
    }
}

fn report(samples: ArtistContinuation) -> LibrarySyncDetailed {
    LibrarySyncDetailed {
        library: Counts {
            tracks: samples.completed * 3,
            candidates: samples.completed * 3,
            classical: 0,
            retired: 0,
        },
        genres: GenreRead::default(),
        genre_continuation: serde_json::from_value(json!({
            "owner":"test-owner", "generation":1, "pending":0
        }))
        .unwrap(),
        albums: serde_json::from_value(json!({
            "owner":"test-owner", "generation":1, "pending":0
        }))
        .unwrap(),
        playlists: PlaylistContinuation {
            owner: Some("test-owner".into()),
            ..Default::default()
        },
        followed_artists: FollowedArtistContinuation::default(),
        artist_samples: samples,
        notes: Vec::new(),
    }
}

fn checkpoint() -> Checkpoint {
    let read = report(samples(70, 50));
    let mut state = Checkpoint::default().observed(&read.playlists, AT);
    state.observed_albums(&read.albums, AT, false);
    state.observed_genres(&read.genre_continuation, AT, false);
    state.observed_artists(&read.artist_samples, AT);
    state
}

#[test]
fn full_refresh_admits_samples_and_restart_finishes_them_without_postponing_full_sync() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path(), AT);
    sync.accepted(
        None,
        &Checkpoint::default(),
        &report(samples(70, 50)),
        PassKind::Full,
        AT,
    )
    .unwrap();
    let first = load(dir.path()).unwrap();
    assert!(sync.take_fresh());
    assert_eq!(first.artist_samples.as_ref().unwrap().completed, 50);
    assert!(matches!(
        first.background_pass(first.artist_retry_at),
        Some(Pass::Artists(Some(_)))
    ));
    let restarted = controller(dir.path(), first.artist_retry_at);
    assert!(!restarted.due(first.artist_retry_at));
    let latest = restarted
        .accepted(
            restarted.last(),
            &first,
            &report(samples(20, 100)),
            PassKind::Artists,
            first.artist_retry_at,
        )
        .unwrap();
    assert_eq!(latest.at, AT);
    assert_eq!(latest.tracks, 300);
    assert!(restarted.take_fresh());
    let saved = load(dir.path()).unwrap();
    let last = controller(dir.path(), saved.artist_retry_at);
    last.accepted(
        last.last(),
        &saved,
        &report(samples(0, 120)),
        PassKind::Artists,
        saved.artist_retry_at,
    )
    .unwrap();
    let done = load(dir.path()).unwrap();
    assert!(!done.artist_due(i64::MAX));
    assert!(done.background_pass(i64::MAX).is_none());
    let status = last.status();
    assert_eq!(status.last.unwrap().at, AT);
    let samples = status.artist_sampling.unwrap();
    assert_eq!((samples.pending, samples.completed), (0, 120));
    assert!(samples.retry_at.is_none());
    assert!(last.due(AT + i64::try_from(MAX_AGE.as_secs()).unwrap()));
}

#[test]
fn admitted_deadlines_and_errors_survive_restart_before_any_new_artist_read() {
    let dir = tempfile::tempdir().unwrap();
    let state = checkpoint();
    let now = state.artist_retry_at;
    let reserved = state.reserve_artists(now);
    assert_eq!(reserved.artist_stalled, 1);
    assert_eq!(reserved.artist_samples, state.artist_samples);
    write(dir.path(), Some(&full()), &reserved).unwrap();
    let interrupted = load(dir.path()).unwrap();
    assert_eq!(interrupted, reserved);
    assert!(!interrupted.artist_due(interrupted.artist_retry_at - 1));
    let sync = controller(dir.path(), now);
    let failure = Failure::new(ErrorCode::Internal, "temporary Spotify failure");
    assert!(
        sync.finish_pass(
            Some(full()),
            &state,
            reserved.clone(),
            PassKind::Artists,
            Err(failure),
        )
        .is_err()
    );
    let restarted = controller(dir.path(), now);
    assert_eq!(load(dir.path()).unwrap(), reserved);
    assert_eq!(restarted.last().unwrap().at, AT);
    assert!(!restarted.take_fresh());
    assert_eq!(
        restarted.status().artist_sampling.unwrap().retry_at,
        Some(reserved.artist_retry_at)
    );
}

#[test]
fn cumulative_success_is_not_new_progress_and_a_new_cohort_resets_failure_backoff() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path(), AT);
    let mut state = checkpoint();
    for stalled in 1..=3 {
        let at = state.artist_retry_at;
        sync.accepted(
            Some(full()),
            &state,
            &report(samples(70, 50)),
            PassKind::Artists,
            at,
        )
        .unwrap();
        state = load(dir.path()).unwrap();
        assert_eq!(state.artist_stalled, stalled);
        assert_eq!(state.artist_retry_at, at + Checkpoint::wait(stalled));
        assert!(!sync.take_fresh(), "old cumulative success is not a publication");
    }
    let mut next = samples(90, 10);
    next.cycle = Some("sample-cycle-b".into());
    let at = state.artist_retry_at;
    sync.accepted(Some(full()), &state, &report(next), PassKind::Full, at)
        .unwrap();
    let saved = load(dir.path()).unwrap();
    assert_eq!(saved.artist_stalled, 0);
    assert_eq!(saved.artist_retry_at, at + Checkpoint::wait(0));
    assert_eq!(saved.artist_samples.unwrap().completed, 10);
    assert_eq!(sync.last().unwrap().at, at);
}

fn continuing_report(state: &Checkpoint, kind: PassKind) -> LibrarySyncDetailed {
    let mut read = report(state.artist_samples.clone().unwrap());
    read.playlists.pending = 2;
    read.playlists.advanced = 1;
    read.albums = state.albums.clone().unwrap();
    read.genre_continuation = state.genres.clone().unwrap();
    read.albums.advanced = 0;
    read.genre_continuation.completed = 0;
    read.followed_artists = state.followed.clone().unwrap();
    match kind {
        PassKind::Albums => read.albums.advanced = 1,
        PassKind::Genres => read.genre_continuation.completed = 1,
        PassKind::Followed => {
            read.followed_artists.advanced = 1;
            read.followed_artists.pages += 1;
        }
        PassKind::Artists => {
            read.artist_samples.pending -= 1;
            read.artist_samples.completed += 1;
        }
        _ => {}
    }
    read
}

#[test]
fn every_reader_gets_a_fair_turn_without_replacing_an_unrelated_sample_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path(), AT);
    let mut state = checkpoint();
    state.pending = 2;
    state.albums = Some(serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":1
    })).unwrap());
    state.genres = Some(serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":5
    })).unwrap());
    state.followed = Some(FollowedArtistContinuation {
        owner: Some("test-owner".into()),
        generation: Some(1),
        pending: true,
        phase: FollowedArtistPhase::Reading,
        pages: 20,
        artists: 1_000,
        ..Default::default()
    });
    let mut kinds = Vec::new();
    for _ in 0..10 {
        let at = [state.retry_at, state.album_retry_at, state.genre_retry_at,
            state.followed_retry_at, state.artist_retry_at].into_iter().max().unwrap();
        let kind = state.background_pass(at).unwrap().kind();
        kinds.push(kind);
        let old_samples = state.artist_samples.clone();
        let deadline = state.artist_retry_at;
        let read = continuing_report(&state, kind);
        sync.accepted(Some(full()), &state, &read, kind, at).unwrap();
        state = load(dir.path()).unwrap();
        if kind != PassKind::Artists {
            assert_eq!(state.artist_samples, old_samples);
            assert_eq!(state.artist_retry_at, deadline);
        }
    }
    assert_eq!(
        kinds,
        [PassKind::Playlists, PassKind::Albums, PassKind::Genres, PassKind::Followed,
            PassKind::Artists, PassKind::Playlists, PassKind::Albums, PassKind::Genres,
            PassKind::Followed, PassKind::Artists]
    );
}

#[test]
fn artist_rate_limits_block_all_readers_and_manual_full_refresh_until_the_shared_deadline() {
    let dir = tempfile::tempdir().unwrap();
    let state = checkpoint();
    let at = state.artist_retry_at;
    let sync = controller(dir.path(), at);
    let reserved = state.reserve_artists(at);
    let read = ReadResult {
        result: Err(fsonos_spotify::SpotifyError::Api {
            status: 429,
            body: "wait".into(),
        }),
        retry_after: 3_600,
        progress: None,
        progress_observed: false,
    };
    assert!(
        sync.finish_pass(Some(full()), &state, reserved, PassKind::Artists, Ok(read))
            .is_err()
    );
    let saved = load(dir.path()).unwrap();
    assert_eq!(saved.not_before, at + 3_600);
    for deadline in [saved.retry_at, saved.album_retry_at, saved.genre_retry_at,
        saved.followed_retry_at, saved.artist_retry_at, saved.recent.retry_at]
    {
        assert!(deadline >= saved.not_before);
    }
    assert!(saved.background_pass(at + 3_599).is_none());
    let restarted = controller(dir.path(), at + 3_599);
    assert_eq!(restarted.run().unwrap_err().code, ErrorCode::NotReady);
    assert_eq!(restarted.last().unwrap().at, AT);
    assert_eq!(
        restarted.status().artist_sampling.unwrap().not_before,
        Some(at + 3_600)
    );
}

#[test]
fn sample_publication_preserves_retained_album_pages_and_genre_rotation() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path(), AT);
    let mut state = checkpoint();
    let partial: AlbumContinuation = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":1, "advanced":1,
        "cursor":"spotify:album:Recording",
        "partial":{"uri":"spotify:album:Recording", "offset":1, "total":2, "pages":1,
            "tracks":[{"disc_number":1, "track_number":1,
                "source_uri":"spotify:track:MovementOne", "title":"Suite I",
                "duration_secs":180}]}
    })).unwrap();
    let genres: GenreContinuation = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":5, "cursor":"Artist050"
    })).unwrap();
    state.observed_albums(&partial, AT, false);
    state.observed_genres(&genres, AT, false);
    let mut read = report(samples(20, 100));
    read.albums = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":2
    })).unwrap();
    read.genre_continuation = serde_json::from_value(json!({
        "owner":"test-owner", "generation":1, "pending":10
    })).unwrap();
    let latest = sync.accepted(
        Some(full()), &state, &read, PassKind::Artists, state.artist_retry_at,
    ).unwrap();
    let kept = load(dir.path()).unwrap();
    assert_eq!(kept.album_retry_at, state.album_retry_at);
    assert_eq!(kept.genre_retry_at, state.genre_retry_at);
    assert_eq!(
        serde_json::to_value(kept.albums.unwrap()).unwrap()["partial"],
        serde_json::to_value(partial).unwrap()["partial"]
    );
    assert_eq!(
        serde_json::to_value(kept.genres.unwrap()).unwrap()["cursor"],
        serde_json::to_value(genres).unwrap()["cursor"]
    );
    assert_eq!(latest.at, AT);
    assert_eq!(latest.retired, 4);
    assert_eq!(latest.genre_artists_pending, 10);
    assert!(latest.notes.contains(&"an unrelated diagnostic".into()));
}

#[test]
fn ownership_conflicts_and_cache_limits_do_not_claim_completion_or_schedule_sample_reads() {
    let dir = tempfile::tempdir().unwrap();
    let state = checkpoint();
    let sync = controller(dir.path(), state.artist_retry_at);
    sync.accepted(
        Some(full()), &state, &report(ArtistContinuation::refresh_required()),
        PassKind::Artists, state.artist_retry_at,
    ).unwrap();
    let changed = load(dir.path()).unwrap();
    assert!(changed.requires_refresh);
    assert!(changed.background_pass(i64::MAX).is_none());
    assert!(sync.due(changed.retry_at));
    assert!(!sync.take_fresh());
    let mut limited = samples(70, 50);
    limited.limited = true;
    sync.accepted(
        Some(full()), &state, &report(limited), PassKind::Artists, state.artist_retry_at,
    ).unwrap();
    let saved = load(dir.path()).unwrap();
    assert!(!saved.artist_due(i64::MAX));
    assert!(saved.background_pass(i64::MAX).is_none());
    let status = sync.status().artist_sampling.unwrap();
    assert!(status.limited);
    assert_eq!(status.pending, 70);
    assert!(status.retry_at.is_none());
    assert_eq!(sync.last().unwrap().at, AT);
}

#[test]
fn older_per_full_read_counts_do_not_admit_a_background_artist_cohort() {
    let dir = tempfile::tempdir().unwrap();
    let mut value = serde_json::to_value(checkpoint()).unwrap();
    for key in ["artist_samples", "artist_stalled", "artist_retry_at"] {
        value.as_object_mut().unwrap().remove(key);
    }
    value["artist_reads_pending"] = json!(500);
    let old: Checkpoint = serde_json::from_value(value).unwrap();
    write(dir.path(), Some(&full()), &old).unwrap();
    let at = AT + i64::try_from(CHECK.as_secs()).unwrap();
    let sync = controller(dir.path(), at);
    assert!(old.artist_samples.is_none());
    assert!(!old.artist_due(i64::MAX));
    assert!(old.background_pass(at).is_none());
    assert!(sync.status().artist_sampling.is_none());
    assert!(!sync.due(at));
}
