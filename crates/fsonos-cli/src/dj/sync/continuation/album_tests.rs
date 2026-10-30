//! Whole-work continuation uses its own durable clock and pool publication.

use super::*;
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::LibrarySync as Counts;
use fsonos_spotify::client::SpotifyConfig;
use fsonos_spotify::genres::GenreRead;
use serde_json::json;
use std::path::PathBuf;

fn directory(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-album-scheduler-{}-{name}-{}",
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

fn full(at: i64) -> SyncedDto {
    SyncedDto {
        at,
        tracks: 5,
        candidates: 5,
        classical: 1,
        retired: 2,
        genre_artists_pending: 0,
        genre_stalled: 0,
        notes: Vec::new(),
    }
}

fn albums(pending: usize, completed: usize, advanced: usize) -> AlbumContinuation {
    serde_json::from_value(json!({
        "owner": "test-owner",
        "generation": 1,
        "pending": pending,
        "completed": completed,
        "advanced": advanced
    }))
    .unwrap()
}

fn partial() -> AlbumContinuation {
    serde_json::from_value(json!({
        "owner": "test-owner",
        "generation": 1,
        "pending": 1,
        "advanced": 8,
        "cursor": "spotify:album:Recording",
        "partial": {
            "uri": "spotify:album:Recording",
            "offset": 8,
            "total": 10,
            "pages": 8,
            "tracks": [{
                "disc_number": 1,
                "track_number": 1,
                "source_uri": "spotify:track:MovementOne",
                "title": "Sonata No. 3: I. Allegro",
                "duration_secs": 180
            }]
        }
    }))
    .unwrap()
}

fn playlists(pending: usize) -> PlaylistContinuation {
    PlaylistContinuation {
        owner: Some("test-owner".into()),
        pending,
        ..PlaylistContinuation::default()
    }
}

fn checkpoint(at: i64, playlist_count: usize) -> Checkpoint {
    let mut state = Checkpoint::default().observed(&playlists(playlist_count), at);
    state.observed_albums(&albums(1, 0, 0), at, false);
    state
}

fn report(albums: AlbumContinuation) -> LibrarySyncDetailed {
    LibrarySyncDetailed {
        library: Counts {
            tracks: 5,
            candidates: 5,
            classical: 1,
            retired: 0,
        },
        genres: GenreRead::default(),
        playlists: playlists(0),
        albums,
        notes: Vec::new(),
    }
}

#[test]
fn old_stamps_probe_albums_at_the_next_check_without_claiming_a_new_full_sync() {
    let dir = directory("upgrade");
    let at = 1_790_000_000;
    super::super::write_stamp(&dir, &full(at)).unwrap();
    let sync = controller(dir.clone(), at + 1);
    assert!(!sync.due(at + 1));
    assert!(matches!(
        load(&dir).unwrap_or_default().background_pass(at + 1),
        Some(Pass::Albums(None))
    ));
    assert_eq!(sync.last(), Some(full(at)));

    let state = Checkpoint::default().observed(&playlists(0), at);
    let mut legacy = serde_json::to_value(&state).unwrap();
    for key in ["albums", "album_stalled", "album_retry_at", "album_turn"] {
        legacy.as_object_mut().unwrap().remove(key);
    }
    let mut envelope = serde_json::to_value(full(at)).unwrap();
    envelope["playlist_continuation"] = legacy;
    std::fs::write(dir.join(STAMP), serde_json::to_string(&envelope).unwrap()).unwrap();
    let restored = load(&dir).unwrap();
    assert!(matches!(
        restored.background_pass(at + 1),
        Some(Pass::Albums(None))
    ));
    assert_eq!(sync.last(), Some(full(at)));
}

#[test]
fn retained_pages_survive_restart_and_only_complete_albums_refresh_the_dj_pool() {
    let dir = directory("partial");
    let at = 1_790_000_000;
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    let before = full(at);
    let state = checkpoint(at, 0);
    write(&dir, Some(&before), &state).unwrap();
    let sync = controller(dir.clone(), at + check);
    let read = report(partial());
    let latest = sync
        .accepted(Some(before), &state, &read, false, true, at + check)
        .unwrap();
    assert_eq!(latest.at, at);
    assert_eq!(latest.retired, 2);
    assert!(
        !sync.take_fresh(),
        "partial metadata cannot complete a work"
    );
    let saved = load(&dir).unwrap();
    assert_eq!(saved.albums, Some(read.albums.clone()));
    assert_eq!(saved.album_stalled, 0);
    assert_eq!(saved.album_retry_at, at + 2 * check);

    let restarted = controller(dir.clone(), saved.album_retry_at);
    assert!(matches!(
        saved.background_pass(saved.album_retry_at),
        Some(Pass::Albums(Some(_)))
    ));
    let complete = report(albums(0, 1, 0));
    let latest = restarted
        .accepted(
            restarted.last(),
            &saved,
            &complete,
            false,
            true,
            saved.album_retry_at,
        )
        .unwrap();
    assert_eq!(latest.at, at, "an album pass is not a full library sync");
    assert_eq!(latest.retired, 2);
    assert!(
        restarted.take_fresh(),
        "the next DJ pick can use the whole work"
    );
    assert!(!restarted.take_fresh(), "one publication, one notification");
    let done = load(&dir).unwrap();
    assert!(!done.album_due(i64::MAX));
    assert!(done.background_pass(i64::MAX).is_none());
    assert!(
        latest
            .notes
            .iter()
            .any(|note| note.contains("1 album list(s) completed"))
    );
}

#[test]
fn a_cache_publication_before_an_interrupted_checkpoint_still_refreshes_the_pool() {
    let dir = directory("published");
    let at = 1_790_000_000;
    let state = checkpoint(at, 0);
    let sync = controller(dir, state.album_retry_at);
    // The previous process published the album then stopped before recording
    // its success. Recounting pending albums must still notify the active DJ.
    let read = report(albums(0, 0, 0));
    sync.accepted(
        Some(full(at)),
        &state,
        &read,
        false,
        true,
        state.album_retry_at,
    )
    .unwrap();
    assert!(sync.take_fresh());
}

#[test]
fn an_account_generation_change_requests_full_refresh_without_a_fresh_pool_signal() {
    let dir = directory("owner");
    let at = 1_790_000_000;
    let state = checkpoint(at, 0);
    let mut changed = albums(0, 1, 0);
    changed.generation = Some(3);
    changed.requires_refresh = true;
    let sync = controller(dir.clone(), state.album_retry_at);
    sync.accepted(
        Some(full(at)),
        &state,
        &report(changed),
        false,
        true,
        state.album_retry_at,
    )
    .unwrap();
    let saved = load(&dir).unwrap();
    assert!(saved.requires_refresh);
    assert!(!saved.album_due(i64::MAX));
    assert!(!sync.take_fresh());
    assert_eq!(sync.last().unwrap().at, at);
    assert!(sync.due(saved.retry_at));
}

#[test]
fn interrupted_album_passes_back_off_independently_and_retain_server_cooldowns() {
    let dir = directory("cooldown");
    let at = 1_790_000_000;
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    let mut state = checkpoint(at, 2);
    state.observed_albums(&partial(), at, false);
    let mut admitted = state.reserve_albums(state.album_retry_at);
    assert_eq!(admitted.stalled, state.stalled);
    assert_eq!(admitted.retry_at, state.retry_at);
    assert_eq!(admitted.album_stalled, 1);
    assert_eq!(admitted.album_retry_at, at + 3 * check);
    admitted.cooldown(at + check, 3_600);
    write(&dir, Some(&full(at)), &admitted).unwrap();
    let restored = load(&dir).unwrap();
    assert_eq!(restored, admitted);
    assert_eq!(restored.albums, state.albums);
    let before_deadline = restored.not_before - 1;
    assert!(restored.background_pass(before_deadline).is_none());
    let sync = controller(dir.clone(), before_deadline);
    assert_eq!(sync.run().unwrap_err().code, ErrorCode::NotReady);
    assert_eq!(load(&dir), Some(admitted));

    let stalled = restored.reserve_albums(restored.album_retry_at);
    assert_eq!(stalled.album_stalled, 2);
    assert_eq!(stalled.stalled, state.stalled);
    assert_eq!(stalled.album_retry_at, restored.album_retry_at + 4 * check);
}

#[test]
fn playlist_progress_keeps_due_album_pages_and_the_two_background_readers_take_turns() {
    let dir = directory("fairness");
    let at = 1_790_000_000;
    let check = i64::try_from(CHECK.as_secs()).unwrap();
    let mut state = checkpoint(at, 2);
    state.observed_albums(&partial(), at, false);
    let album_deadline = state.album_retry_at;
    let sync = controller(dir.clone(), album_deadline);
    let mut read = report(albums(1, 0, 0));
    read.playlists = playlists(1);
    read.playlists.advanced = 150;
    sync.accepted(Some(full(at)), &state, &read, false, false, album_deadline)
        .unwrap();
    let next = load(&dir).unwrap();
    assert_eq!(next.album_retry_at, album_deadline);
    assert_eq!(next.album_stalled, 0);
    assert_eq!(
        serde_json::to_value(next.albums.as_ref().unwrap()).unwrap()["partial"],
        serde_json::to_value(partial()).unwrap()["partial"]
    );
    assert!(next.album_turn);
    assert!(matches!(
        next.background_pass(next.retry_at),
        Some(Pass::Albums(Some(_)))
    ));

    let mut progressed = partial();
    progressed.advanced = 1;
    sync.accepted(
        sync.last(),
        &next,
        &report(progressed),
        false,
        true,
        next.retry_at,
    )
    .unwrap();
    let after_album = load(&dir).unwrap();
    assert!(!after_album.album_turn);
    assert_eq!(after_album.retry_at, next.retry_at);
    assert_eq!(after_album.album_retry_at, next.retry_at + check);
    assert!(matches!(
        after_album.background_pass(after_album.album_retry_at),
        Some(Pass::Playlists(_))
    ));
}
