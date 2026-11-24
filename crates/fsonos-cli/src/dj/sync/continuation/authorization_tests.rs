//! New consent and ordinary credential refresh have different scheduling and
//! selection consequences, including after restarts and interrupted reads.

use super::*;
use crate::dj::sync::authorization;
use fsonos_core::clock::FakeClock;
use fsonos_spotify::SpotifyError;
use fsonos_spotify::cache::LibrarySync as Counts;
use fsonos_spotify::client::{CachedToken, SCOPE, SpotifyConfig, TokenCache};
use fsonos_spotify::genres::GenreRead;

const AT: i64 = 1_790_000_000;

fn controller(dir: &Path) -> LibrarySync {
    LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        dir.to_owned(),
    )
    .unwrap()
    .with_clock(Box::new(FakeClock::new(
        chrono::DateTime::from_timestamp(AT, 0)
            .unwrap()
            .fixed_offset(),
    )))
}

fn sign_in(dir: &Path) -> String {
    let cache = TokenCache::in_data_dir(dir);
    cache
        .store(&CachedToken {
            access_token: "fixture-access".into(),
            refresh_token: "fixture-refresh".into(),
            expires_at: i64::MAX,
            scope: SCOPE.into(),
        })
        .unwrap();
    cache.authorization_id().unwrap().unwrap()
}

fn completed() -> LibrarySyncDetailed {
    LibrarySyncDetailed {
        library: Counts {
            tracks: 10,
            candidates: 10,
            classical: 0,
            retired: 0,
        },
        genres: GenreRead::default(),
        genre_continuation: GenreContinuation::default(),
        followed_artists: FollowedArtistContinuation::default(),
        artist_samples: ArtistContinuation::default(),
        playlists: PlaylistContinuation {
            owner: Some("fixture-owner".into()),
            ..PlaylistContinuation::default()
        },
        albums: AlbumContinuation::default(),
        notes: Vec::new(),
    }
}

fn accepted(dir: &Path, sync: &LibrarySync) -> Checkpoint {
    let authorization = sign_in(dir);
    let mut checkpoint = Checkpoint::default();
    assert!(checkpoint.observe_authorization(Some(authorization), AT - 1));
    sync.accepted(None, &checkpoint, &completed(), PassKind::Full, AT - 1)
        .unwrap();
    load(dir).unwrap()
}

#[test]
fn replacing_an_already_signed_in_grant_makes_a_recent_library_due() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path());
    let previous = accepted(dir.path(), &sync);
    assert!(!sync.due(AT));
    assert!(authorization::selection(dir.path()).is_ok());
    let last = sync.last().unwrap();

    let replacement = sign_in(dir.path());
    assert_ne!(previous.authorization_id.as_ref(), Some(&replacement));
    assert!(
        sync.due(AT),
        "a fresh full timestamp cannot postpone new consent"
    );
    assert_eq!(
        authorization::selection(dir.path()).unwrap_err().code,
        ErrorCode::NotReady
    );
    assert!(sync.observe_authorization().unwrap());
    let observed = load(dir.path()).unwrap();
    assert_eq!(observed.authorization_id.as_ref(), Some(&replacement));
    assert!(observed.accepted_authorization_id.is_none());
    assert!(observed.requires_refresh && observed.intake_pending);
    assert!(
        !sync.refresh_recent_if_due().unwrap(),
        "unaccepted grants cannot start a listening read"
    );
    assert_eq!(sync.last().unwrap(), last);
    assert!(
        controller(dir.path()).due(AT),
        "new consent survives restart"
    );

    let reserved = observed.reserve_full(AT);
    write(dir.path(), Some(&last), &reserved).unwrap();
    assert!(!sync.observe_authorization().unwrap());
    assert_eq!(
        load(dir.path()).unwrap(),
        reserved,
        "same grant retains bounded backoff"
    );
}

#[test]
fn new_consent_retires_old_work_but_preserves_the_application_cooldown() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path());
    let mut old = accepted(dir.path(), &sync);
    old.pending = 12;
    old.stalled = 8;
    old.retry_at = AT + 86_400;
    old.not_before = AT + 600;
    old.recent.retry_at = AT + 20_000;
    write(dir.path(), sync.last().as_ref(), &old).unwrap();

    let replacement = sign_in(dir.path());
    assert!(!sync.due(AT + 599));
    assert!(sync.observe_authorization().unwrap());
    let saved = load(dir.path()).unwrap();
    assert_eq!(saved.authorization_id.as_ref(), Some(&replacement));
    assert_eq!((saved.pending, saved.stalled), (0, 0));
    assert!(saved.owner.is_none() && saved.artist_samples.is_none());
    assert_eq!(saved.recent, RecentCheckpoint::default());
    assert_eq!((saved.not_before, saved.retry_at), (AT + 600, AT + 600));
    assert!(!sync.due(AT + 599));
    assert!(
        sync.due(AT + 600),
        "old grant backoff cannot delay recovery"
    );
    assert!(
        sync.refresh_recent_if_due().is_ok(),
        "unaccepted grant cannot dispatch listening I/O"
    );
}

#[test]
fn an_old_completed_or_pending_read_cannot_acknowledge_a_new_sign_in() {
    for pending in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let sync = controller(dir.path());
        let mut previous = accepted(dir.path(), &sync);
        previous.accepted_authorization_id = None;
        previous.requires_refresh = true;
        let last = sync.last().unwrap();
        let replacement = sign_in(dir.path());
        let read = ReadResult {
            result: if pending {
                Err(SpotifyError::LibraryPending {
                    pages: 512,
                    tracks: 20_000,
                })
            } else {
                Ok(completed())
            },
            retry_after: 600,
            progress: None,
            progress_observed: false,
        };
        sync.take_fresh();
        let failure = sync
            .finish_pass(
                Some(last.clone()),
                &previous,
                previous.reserve_full(AT),
                PassKind::Full,
                Ok(read),
            )
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::NotReady);
        let saved = load(dir.path()).unwrap();
        assert_eq!(saved.authorization_id.as_ref(), Some(&replacement));
        assert!(saved.accepted_authorization_id.is_none());
        assert!(saved.intake_pending && saved.requires_refresh);
        assert_eq!(saved.not_before, AT + 600);
        assert_eq!(sync.last().unwrap(), last);
        assert!(!sync.take_fresh());
        assert!(authorization::selection(dir.path()).is_err());
    }
}

#[test]
fn only_a_completed_full_read_can_admit_new_selections() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path());
    let receipt = sign_in(dir.path());
    sync.observe_authorization().unwrap();
    let checkpoint = load(dir.path()).unwrap();

    sync.finish_pass(
        None,
        &checkpoint,
        checkpoint.reserve_full(AT),
        PassKind::Full,
        Ok(ReadResult {
            result: Err(SpotifyError::LibraryPending {
                pages: 512,
                tracks: 20_000,
            }),
            retry_after: 0,
            progress: None,
            progress_observed: false,
        }),
    )
    .unwrap();
    assert!(authorization::selection(dir.path()).is_err());
    let checkpoint = load(dir.path()).unwrap();
    sync.finish_pass(
        None,
        &checkpoint,
        checkpoint.reserve_full(AT),
        PassKind::Full,
        Ok(ReadResult {
            result: Ok(completed()),
            retry_after: 0,
            progress: None,
            progress_observed: false,
        }),
    )
    .unwrap();
    assert_eq!(authorization::selection(dir.path()).unwrap(), Some(receipt));
    assert!(sync.take_fresh());
    assert!(!controller(dir.path()).due(AT));
}

#[test]
fn removed_credentials_remain_fenced_even_before_the_first_full_publication() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path());
    assert_eq!(authorization::selection(dir.path()).unwrap(), None);
    sign_in(dir.path());
    sync.observe_authorization().unwrap();
    std::fs::remove_file(TokenCache::in_data_dir(dir.path()).path()).unwrap();
    sync.observe_authorization().unwrap();
    let saved = load(dir.path()).unwrap();
    assert!(saved.authorization_observed && saved.authorization_id.is_none());
    assert_eq!(
        authorization::selection(dir.path()).unwrap_err().code,
        ErrorCode::NotReady
    );
    assert!(!controller(dir.path()).observe_authorization().unwrap());
    assert!(authorization::selection(dir.path()).is_err());
}

#[test]
fn legacy_library_stamps_require_one_new_receipt_but_still_read_the_previous_success() {
    let dir = tempfile::tempdir().unwrap();
    let sync = controller(dir.path());
    let last = full_result(&completed(), AT - 1);
    super::super::write_stamp(dir.path(), &last).unwrap();
    sign_in(dir.path());
    assert_eq!(sync.last(), Some(last.clone()));
    assert!(sync.due(AT));
    assert!(authorization::selection(dir.path()).is_err());
    assert!(sync.observe_authorization().unwrap());
    assert_eq!(sync.last(), Some(last));
}

fn live_watch_reauthorization(hold_writer: bool) {
    use fsonos_core::clock::Clock;
    use std::sync::{Arc, atomic::AtomicBool, mpsc};
    use std::time::Duration;

    // Observe the real worker's boundary without sleeping to guess whether
    // its first check ran. Wall and elapsed time never advance in this test.
    struct TickClock(mpsc::Sender<()>);
    impl Clock for TickClock {
        fn now(&self) -> chrono::DateTime<chrono::FixedOffset> {
            chrono::DateTime::from_timestamp(AT, 0)
                .unwrap()
                .fixed_offset()
        }

        fn elapsed_secs(&self) -> i64 {
            let _ = self.0.send(());
            AT
        }
    }

    let dir = tempfile::tempdir().unwrap();
    let (sent, ticks) = mpsc::channel();
    let sync = Arc::new(controller(dir.path()).with_clock(Box::new(TickClock(sent))));
    let mut previous = accepted(dir.path(), &sync);
    previous.not_before = AT + 3_600;
    write(dir.path(), sync.last().as_ref(), &previous).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let worker = super::super::watch(Some(Arc::clone(&sync)), Arc::clone(&stop))
        .unwrap()
        .unwrap();
    let observed = (|| -> Result<(String, Checkpoint), String> {
        for _ in 0..2 {
            ticks
                .recv_timeout(Duration::from_secs(5))
                .map_err(|error| error.to_string())?;
        }
        let writer = hold_writer.then(|| sync.try_claim_directory().unwrap().unwrap());
        let replacement = sign_in(dir.path());
        if hold_writer {
            // Let the real watcher hit the held file lock on multiple ticks.
            // Seeing a busy writer must not consume the pending consent wake.
            for _ in 0..3 {
                ticks
                    .recv_timeout(Duration::from_secs(5))
                    .map_err(|error| error.to_string())?;
            }
            assert_eq!(
                load(dir.path()).unwrap().authorization_id,
                previous.authorization_id
            );
        }
        drop(writer);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            ticks
                .recv_timeout(deadline.saturating_duration_since(std::time::Instant::now()))
                .map_err(|error| error.to_string())?;
            if let Some(saved) = load(dir.path())
                && saved.authorization_id.as_ref() == Some(&replacement)
            {
                return Ok((replacement, saved));
            }
        }
    })();
    stop.store(true, Ordering::Release);
    worker.join().unwrap();
    let (replacement, saved) = observed.unwrap();
    assert_eq!(saved.authorization_id.as_ref(), Some(&replacement));
    assert!(saved.accepted_authorization_id.is_none());
    assert_eq!(
        saved.not_before,
        AT + 3_600,
        "wake cannot bypass Spotify's cooldown"
    );
    assert!(saved.requires_refresh && saved.intake_pending);
    assert_eq!(sync.now(), AT, "the fifteen-minute deadline never elapsed");
}

#[test]
fn the_live_daemon_watch_wakes_for_reauthorization_before_its_periodic_deadline() {
    live_watch_reauthorization(false);
}

#[test]
fn reauthorization_wake_survives_a_busy_refresh_writer() {
    live_watch_reauthorization(true);
}
