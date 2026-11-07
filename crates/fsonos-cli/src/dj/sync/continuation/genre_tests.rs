//! Genre work has its own durable progress, but shares admission and cooldown.

use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::LibrarySync as Counts;
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use serde_json::json;
use std::path::PathBuf;

const AT: i64 = 1_790_000_000;

fn directory(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-genre-scheduler-{}-{name}-{}",
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
    LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        dir,
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
        tracks: 125,
        candidates: 125,
        classical: 1,
        retired: 4,
        genre_artists_pending: 125,
        genre_stalled: 0,
        notes: vec![
            "125 artists' genres wait for the next sync".into(),
            "1 album couldn't be read (not found)".into(),
        ],
    }
}

fn genres(pending: usize, completed: usize, cursor: &str) -> GenreContinuation {
    serde_json::from_value(json!({
        "owner": "test-owner", "generation": 1,
        "pending": pending, "completed": completed, "cursor": cursor
    }))
    .unwrap()
}

fn albums(pending: usize) -> AlbumContinuation {
    serde_json::from_value(json!({
        "owner": "test-owner", "generation": 1, "pending": pending
    }))
    .unwrap()
}

fn state(playlists: usize, album_count: usize, pending: usize) -> Checkpoint {
    let mut state = Checkpoint::default().observed(
        &PlaylistContinuation {
            owner: Some("test-owner".into()),
            pending: playlists,
            ..PlaylistContinuation::default()
        },
        AT,
    );
    state.observed_albums(&albums(album_count), AT, false);
    state.observed_genres(&genres(pending, 0, "Artist000"), AT, false);
    state
}

fn report(genres: GenreContinuation, albums: AlbumContinuation) -> LibrarySyncDetailed {
    LibrarySyncDetailed {
        library: Counts {
            tracks: 125,
            candidates: 125,
            classical: 1,
            retired: 0,
        },
        genres: GenreRead::default(),
        genre_continuation: genres,
        followed_artists: fsonos_spotify::taste::FollowedArtistContinuation::default(),
        playlists: PlaylistContinuation {
            owner: Some("test-owner".into()),
            pending: 2,
            advanced: 1,
            ..PlaylistContinuation::default()
        },
        albums,
        notes: Vec::new(),
    }
}

#[test]
fn artist_progress_survives_restart_without_repeating_or_postponing_full_sync() {
    let dir = directory("restart");
    let checkpoint = state(0, 0, 125);
    write(&dir, Some(&full()), &checkpoint).unwrap();
    let at = checkpoint.genre_retry_at;
    let sync = controller(dir.clone(), at);
    assert!(
        !sync.due(at),
        "pending artists no longer force a full library read"
    );
    assert!(matches!(
        checkpoint.background_pass(at),
        Some(Pass::Genres(Some(_)))
    ));
    let read = report(genres(75, 50, "Artist050"), albums(0));
    let latest = sync
        .accepted(Some(full()), &checkpoint, &read, PassKind::Genres, at)
        .unwrap();
    assert_eq!(latest.at, AT);
    assert_eq!(latest.retired, 4);
    assert_eq!(latest.genre_artists_pending, 75);
    assert!(!latest.notes.iter().any(|note| legacy_genre_note(note)));
    assert!(
        latest
            .notes
            .iter()
            .any(|note| note == "1 album couldn't be read (not found)")
    );
    assert!(sync.take_fresh());
    assert!(!sync.take_fresh());

    let saved = load(&dir).unwrap();
    assert_eq!(saved.genres, Some(read.genre_continuation));
    let restarted = controller(dir, saved.genre_retry_at);
    let status = restarted.status();
    let enrichment = status.genre_enrichment.unwrap();
    assert_eq!((enrichment.pending, enrichment.completed), (75, 50));
    assert_eq!(enrichment.retry_at, Some(saved.genre_retry_at));
    assert_eq!(status.last.unwrap().at, AT);
    match saved.background_pass(saved.genre_retry_at) {
        Some(Pass::Genres(Some(progress))) => {
            assert_eq!(
                serde_json::to_value(progress).unwrap()["cursor"],
                "Artist050"
            );
        }
        _ => panic!("the saved genre cursor must reach the next pass"),
    }
    assert!(!restarted.due(saved.genre_retry_at));
    assert!(restarted.due(AT + i64::try_from(MAX_AGE.as_secs()).unwrap()));
    let complete = report(genres(0, 75, "Artist125"), albums(0));
    restarted
        .accepted(
            restarted.last(),
            &saved,
            &complete,
            PassKind::Genres,
            saved.genre_retry_at,
        )
        .unwrap();
    let done = load(&restarted.data_dir).unwrap();
    assert!(done.background_pass(i64::MAX).is_none());
    assert_eq!(restarted.last().unwrap().at, AT);
    assert_eq!(restarted.last().unwrap().genre_artists_pending, 0);
    assert_eq!(restarted.status().genre_enrichment.unwrap().pending, 0);
    assert!(restarted.take_fresh());
}

#[test]
fn admission_and_rate_limits_survive_restart_and_block_all_spotify_readers() {
    let dir = directory("cooldown");
    let checkpoint = state(2, 1, 125);
    let at = checkpoint.genre_retry_at;
    let mut admitted = checkpoint.reserve_genres(at);
    assert_eq!(admitted.stalled, checkpoint.stalled);
    assert_eq!(admitted.album_stalled, checkpoint.album_stalled);
    assert_eq!(admitted.genre_stalled, 1);
    assert!(admitted.genre_retry_at > at);
    admitted.cooldown(at, 3_600);
    write(&dir, Some(&full()), &admitted).unwrap();
    let restored = load(&dir).unwrap();
    assert_eq!(restored, admitted);
    assert!(restored.background_pass(at + 3_599).is_none());
    let restarted = controller(dir.clone(), at + 3_599);
    let error = restarted.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(error.detail.contains("no refresh request was sent"));
    assert_eq!(load(&dir), Some(admitted));
    let status = restarted.status().genre_enrichment.unwrap();
    assert_eq!(status.not_before, Some(at + 3_600));
    assert_eq!(restarted.last().unwrap().at, AT);
}

#[test]
fn playlists_albums_and_genres_get_turns_even_when_every_reader_has_more_work() {
    let dir = directory("fairness");
    let mut checkpoint = state(2, 1, 125);
    let sync = controller(dir.clone(), AT);
    let mut kinds = Vec::new();
    for cycle in 0..6 {
        let at = checkpoint
            .retry_at
            .max(checkpoint.album_retry_at)
            .max(checkpoint.genre_retry_at);
        let kind = checkpoint.background_pass(at).unwrap().kind();
        kinds.push(kind);
        let mut read = report(genres(125 - cycle, 0, "Artist050"), albums(1));
        match kind {
            PassKind::Albums => read.albums.advanced = 1,
            PassKind::Genres => read.genre_continuation.completed = 1,
            _ => {}
        }
        sync.accepted(Some(full()), &checkpoint, &read, kind, at)
            .unwrap();
        checkpoint = load(&dir).unwrap();
    }
    assert_eq!(
        kinds,
        [
            PassKind::Playlists,
            PassKind::Albums,
            PassKind::Genres,
            PassKind::Playlists,
            PassKind::Albums,
            PassKind::Genres,
        ]
    );
}

#[test]
fn other_library_observations_keep_a_due_artist_cursor_and_its_backoff() {
    let dir = directory("keep-cursor");
    let mut checkpoint = state(2, 1, 125);
    checkpoint.genres = Some(genres(125, 0, "Artist050"));
    checkpoint.genre_stalled = 3;
    let deadline = checkpoint.genre_retry_at;
    let sync = controller(dir.clone(), deadline);
    let read = report(genres(126, 0, ""), albums(1));
    let latest = sync
        .accepted(
            Some(full()),
            &checkpoint,
            &read,
            PassKind::Playlists,
            deadline,
        )
        .unwrap();
    assert_eq!(latest.genre_artists_pending, 126);
    assert_eq!(latest.genre_stalled, 3);
    let saved = load(&dir).unwrap();
    assert_eq!(saved.genre_retry_at, deadline);
    assert_eq!(saved.genre_stalled, 3);
    assert_eq!(
        serde_json::to_value(saved.genres.unwrap()).unwrap()["cursor"],
        "Artist050"
    );
}

#[test]
fn genres_can_reveal_an_album_backlog_without_discarding_retained_album_pages() {
    let dir = directory("new-work");
    let checkpoint = state(0, 0, 125);
    let at = checkpoint.genre_retry_at;
    let sync = controller(dir.clone(), at);
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(genres(75, 50, "Artist050"), albums(1)),
        PassKind::Genres,
        at,
    )
    .unwrap();
    let mut saved = load(&dir).unwrap();
    assert_eq!(saved.albums.as_ref().unwrap().pending, 1);
    assert!(matches!(
        saved.background_pass(saved.album_retry_at),
        Some(Pass::Albums(Some(_)))
    ));
    assert_eq!(sync.last().unwrap().at, AT);

    let partial: AlbumContinuation = serde_json::from_value(json!({
        "owner": "test-owner", "generation": 1, "pending": 1, "advanced": 8,
        "cursor": "spotify:album:Recording",
        "partial": { "uri": "spotify:album:Recording", "offset": 8,
            "total": 10, "pages": 8, "tracks": [{
                "disc_number": 1, "track_number": 1,
                "source_uri": "spotify:track:MovementOne",
                "title": "River Suite: I. Flowing", "duration_secs": 180
            }] }
    }))
    .unwrap();
    saved.observed_albums(&partial, at, false);
    let deadline = saved.album_retry_at;
    sync.accepted(
        sync.last(),
        &saved,
        &report(genres(25, 50, "Artist100"), albums(1)),
        PassKind::Genres,
        saved.genre_retry_at,
    )
    .unwrap();
    let kept = load(&dir).unwrap();
    assert_eq!(kept.album_retry_at, deadline);
    assert_eq!(
        serde_json::to_value(kept.albums.unwrap()).unwrap()["partial"],
        serde_json::to_value(partial).unwrap()["partial"]
    );
}

#[test]
fn owner_changes_request_full_refresh_and_failed_passes_do_not_publish_a_fresh_pool() {
    let dir = directory("owner");
    let checkpoint = state(0, 0, 125);
    let at = checkpoint.genre_retry_at;
    let sync = controller(dir.clone(), at);
    sync.accepted(
        Some(full()),
        &checkpoint,
        &report(genres(125, 0, "Artist050"), albums(0)),
        PassKind::Genres,
        at,
    )
    .unwrap();
    assert!(
        !sync.take_fresh(),
        "failed lookups do not change the candidate pool"
    );
    let saved = load(&dir).unwrap();
    assert_eq!(saved.genre_stalled, 1);
    assert!(saved.genre_retry_at > at);

    let mut changed = genres(0, 0, "");
    changed.owner = Some("new-owner".into());
    changed.generation = Some(2);
    changed.requires_refresh = true;
    sync.accepted(
        sync.last(),
        &saved,
        &report(changed, albums(0)),
        PassKind::Genres,
        saved.genre_retry_at,
    )
    .unwrap();
    let saved = load(&dir).unwrap();
    assert!(saved.requires_refresh);
    assert!(saved.background_pass(i64::MAX).is_none());
    assert!(!sync.take_fresh());
    assert!(sync.due(saved.retry_at));
    assert!(sync.status().genre_enrichment.unwrap().requires_refresh);
    assert_eq!(sync.last().unwrap().at, AT);
}
