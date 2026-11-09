//! A listening status belongs to the local account receipt, even when the
//! network read fails or a full refresh has not reached publication.

use super::*;
use crate::dj::sync::continuation::PassKind;
use crate::dj::sync::{ReadResult, SyncedDto};
use fsonos_core::clock::FakeClock;
use fsonos_spotify::cache::{SavedLibraryPhase, SavedLibraryProgress};
use fsonos_spotify::client::SpotifyConfig;
use std::path::PathBuf;

const NOW: i64 = 1_790_000_000;

struct Fixture {
    directory: PathBuf,
    sync: LibrarySync,
    store: SqliteStore,
    saved: Checkpoint,
    owner: SpotifyLibraryOwner,
}

fn full() -> SyncedDto {
    serde_json::from_value(serde_json::json!({
        "at":NOW - 100,"tracks":20,"candidates":18,"classical":2,"retired":1
    }))
    .unwrap()
}

fn fixture(name: &str) -> Fixture {
    let directory = std::env::temp_dir().join(format!(
        "fsonos-listening-owner-{}-{name}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&directory).unwrap();
    let sync = LibrarySync::new(
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        },
        directory.clone(),
    )
    .unwrap()
    .with_clock(Box::new(FakeClock::new(
        chrono::DateTime::from_timestamp(NOW, 0)
            .unwrap()
            .fixed_offset(),
    )));
    let mut store = SqliteStore::open(&directory.join(crate::daemon::DB_FILE)).unwrap();
    let (owner, _) = store.adopt_spotify_library_owner("account-a").unwrap();
    let mut saved = Checkpoint::default();
    saved.recent.accept(
        RecentListening {
            owner: Some(owner.owner_id.clone()),
            generation: Some(owner.generation),
            checked_at: Some(NOW - 100),
            received: 3,
            accepted: 2,
            skipped: 1,
            ..RecentListening::default()
        },
        NOW - 100,
    );
    saved.recent.error = Some("an older observation failed".into());
    write(&directory, Some(&full()), &saved).unwrap();
    Fixture {
        directory,
        sync,
        store,
        saved,
        owner,
    }
}

fn read_result(owner: &SpotifyLibraryOwner, pending: bool) -> ReadResult {
    ReadResult {
        result: Err(if pending {
            SpotifyError::LibraryPending {
                pages: 2,
                tracks: 40,
            }
        } else {
            SpotifyError::Api {
                status: 429,
                body: "busy".into(),
            }
        }),
        retry_after: if pending { 0 } else { 3_600 },
        progress: Some(SavedLibraryProgress {
            owner: owner.owner_id.clone(),
            base_owner: Some(owner.owner_id.clone()),
            base_generation: Some(owner.generation),
            pages: 2,
            tracks: 40,
            verified_pages: 2,
            phase: SavedLibraryPhase::Ready,
        }),
        progress_observed: true,
    }
}

#[test]
fn another_adoption_clears_status_even_when_the_full_read_is_pending_or_failed() {
    for pending in [false, true] {
        let mut fixture = fixture("post-read");
        let (owner, _) = fixture
            .store
            .adopt_spotify_library_owner("account-b")
            .unwrap();
        let listening = vec![("spotify:track:Current".into(), NOW)];
        assert!(
            fixture
                .store
                .merge_spotify_heard(&owner, &listening)
                .unwrap()
        );
        let result = fixture.sync.finish_pass(
            Some(full()),
            &fixture.saved,
            fixture.saved.reserve_full(NOW),
            PassKind::Full,
            Ok(read_result(&owner, pending)),
        );
        assert_eq!(result.is_ok(), pending);
        let saved = load(&fixture.directory).unwrap();
        assert!(saved.recent.report.is_none() && saved.recent.error.is_none());
        assert!(saved.intake_pending);
        assert_eq!(
            saved.intake.as_ref().unwrap().base_owner.as_deref(),
            Some("account-b")
        );
        let status = fixture.sync.status();
        let recent = status.recent_listening.unwrap();
        assert_eq!(
            (recent.checked_at, recent.accepted, recent.received),
            (None, 0, 0)
        );
        assert_eq!(status.last, Some(full()));
        assert_eq!(fixture.store.spotify_heard().unwrap(), listening);
        assert!(!fixture.sync.take_fresh());
        if !pending {
            assert_eq!(recent.not_before, Some(NOW + 3_600));
        }
    }
}

#[test]
fn a_failed_full_read_preserves_the_matching_accounts_previous_listening() {
    let fixture = fixture("same-owner");
    assert!(
        fixture
            .sync
            .finish_pass(
                Some(full()),
                &fixture.saved,
                fixture.saved.reserve_full(NOW),
                PassKind::Full,
                Ok(read_result(&fixture.owner, false)),
            )
            .is_err()
    );
    let saved = load(&fixture.directory).unwrap();
    assert_eq!(saved.recent.report, fixture.saved.recent.report);
    assert_eq!(saved.recent.error, fixture.saved.recent.error);
    let recent = fixture.sync.status().recent_listening.unwrap();
    assert_eq!((recent.checked_at, recent.accepted), (Some(NOW - 100), 2));
    assert_eq!(recent.not_before, Some(NOW + 3_600));
}

#[test]
fn a_background_check_fences_status_during_cooldown_without_a_network_read() {
    let mut fixture = fixture("cooldown");
    fixture.saved.cooldown(NOW, 3_600);
    let deadline = fixture.saved.recent.retry_at;
    write(&fixture.directory, Some(&full()), &fixture.saved).unwrap();
    fixture
        .store
        .adopt_spotify_library_owner("account-b")
        .unwrap();
    assert!(!fixture.sync.signed_in());
    assert!(fixture.sync.refresh_recent_if_due().unwrap());
    let saved = load(&fixture.directory).unwrap();
    assert!(saved.recent.report.is_none() && saved.recent.error.is_none());
    assert_eq!(saved.recent.retry_at, deadline);
    assert_eq!(saved.not_before, NOW + 3_600);
    assert_eq!(fixture.sync.last(), Some(full()));
    assert!(!fixture.sync.status().running);
    assert!(!fixture.sync.take_fresh());
}

#[test]
fn rejected_manual_admission_clears_old_counts_and_releases_the_worker_slot() {
    let mut fixture = fixture("admission");
    fixture.saved.cooldown(NOW, 3_600);
    write(&fixture.directory, Some(&full()), &fixture.saved).unwrap();
    fixture
        .store
        .adopt_spotify_library_owner("account-b")
        .unwrap();
    let error = fixture.sync.run().unwrap_err();
    assert_eq!(error.code, ErrorCode::NotReady);
    assert!(load(&fixture.directory).unwrap().recent.report.is_none());
    assert!(!fixture.sync.status().running);
    assert!(!fixture.sync.running().recent);
    assert_eq!(fixture.sync.last(), Some(full()));
}

#[test]
fn a_returning_account_needs_its_new_generation() {
    let mut fixture = fixture("returning-account");
    fixture
        .store
        .adopt_spotify_library_owner("account-b")
        .unwrap();
    let (returned, _) = fixture
        .store
        .adopt_spotify_library_owner("account-a")
        .unwrap();
    assert_ne!(returned.generation, fixture.owner.generation);
    assert!(fixture.sync.fence_recent_status(&mut fixture.saved));
    assert!(fixture.saved.recent.report.is_none());
}

#[test]
fn unknown_ownership_discards_personal_status_but_keeps_scope_and_retry_information() {
    let mut saved = RecentCheckpoint::default();
    saved.accept(
        RecentListening {
            owner: Some("account-a".into()),
            generation: Some(1),
            checked_at: Some(NOW),
            ..RecentListening::default()
        },
        NOW,
    );
    let deadline = saved.retry_at;
    assert!(saved.retain_owner(None));
    assert_eq!(saved.retry_at, deadline);
    saved.accept(
        RecentListening {
            missing_scope: true,
            ..RecentListening::default()
        },
        NOW,
    );
    assert!(!saved.retain_owner(None));
    assert!(saved.report.unwrap().missing_scope);
}
