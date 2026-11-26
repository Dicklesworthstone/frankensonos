//! Durable journal transitions use real core records captured before any
//! announcement mutation. These tests never dispatch to physical speakers.

use super::Journal;
use fsonos_api::surface::announce::{AnnouncementContext, AnnouncementJournal};
use fsonos_core::announce::{AnnouncementRecovery, HouseholdAnnouncement};
use fsonos_types::PlayerId;
use std::fs::{self, OpenOptions};
use std::sync::{Arc, Barrier};
use std::time::{Duration, Instant};

const FILE: &str = "announcement-recovery.json";
const LOCK: &str = "announcement-recovery.lock";

fn directory() -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix("fsonos-announcement-journal-")
        .tempdir()
        .unwrap()
}

fn context(n: u64) -> AnnouncementContext {
    AnnouncementContext {
        id: format!("{n:032x}"),
        client: "local-test-owner".into(),
        kind: "chime".into(),
        rooms: vec!["Alpha".into()],
        created_at: 1,
        interrupted_sleeps: Vec::new(),
        interrupted_sleep_deadlines: std::collections::BTreeMap::new(),
    }
}

fn change_record(
    original: &AnnouncementRecovery,
    change: impl FnOnce(&mut serde_json::Value),
) -> AnnouncementRecovery {
    let mut saved = serde_json::to_value(original).unwrap();
    change(&mut saved);
    let record: AnnouncementRecovery = serde_json::from_value(saved).unwrap();
    assert!(
        record.valid(),
        "the journal fixture must be structurally valid"
    );
    record
}

fn recovery(n: u64) -> AnnouncementRecovery {
    change_record(fixture::initial(), |saved| {
        saved["id"] = format!("recovery-{n}").into();
        saved["created_at"] = (n + 1).into();
    })
}

fn report(text: &str) -> HouseholdAnnouncement {
    HouseholdAnnouncement {
        lead: PlayerId("test-lead".into()),
        rooms: vec!["Alpha".into()],
        levels: Vec::new(),
        outcome: Err(text.into()),
        restored: Vec::new(),
        restore_failed: Vec::new(),
    }
}

mod fixture {
    use super::AnnouncementRecovery;
    use fsonos_core::announce::{
        AnnouncementCallbacks, AnnouncementCheckpoint, AnnouncementControl, Announcer, Clip,
        HouseholdAnnouncement,
    };
    use fsonos_core::{CoreError, HouseholdState, resolve_room};
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
    use std::sync::{Mutex, OnceLock};
    use std::time::Duration;

    #[derive(Default)]
    struct Capture(Mutex<Option<AnnouncementRecovery>>);

    impl AnnouncementCheckpoint for Capture {
        fn save(&self, record: &AnnouncementRecovery) -> Result<(), CoreError> {
            *self.0.lock().unwrap() = Some(record.clone());
            Err(CoreError::Store(
                "fixture stops before the first mutation".into(),
            ))
        }

        fn claim(&self, _: &AnnouncementRecovery) -> Result<bool, CoreError> {
            panic!("a rejected preparation checkpoint cannot claim cleanup");
        }

        fn complete(
            &self,
            _: &AnnouncementRecovery,
            _: &HouseholdAnnouncement,
        ) -> Result<(), CoreError> {
            panic!("a rejected preparation checkpoint cannot complete cleanup");
        }

        fn current(&self, _: &AnnouncementRecovery) -> bool {
            false
        }
    }

    pub(super) fn initial() -> &'static AnnouncementRecovery {
        static INITIAL: OnceLock<AnnouncementRecovery> = OnceLock::new();
        INITIAL.get_or_init(|| {
            let sim = SimHousehold::builder()
                .s1([SimPlayerSpec::new("Alpha", SimModel::Play5Gen1)])
                .spawn()
                .unwrap();
            let lan = sim.lan();
            let mut household = HouseholdState::default();
            household.apply_topology(
                &get_zone_group_state(&lan, sim.player("Alpha").unwrap().ip).unwrap(),
            );
            let households = [household];
            let target = resolve_room(&households, "Alpha").unwrap();
            let capture = Capture::default();
            let _result = Announcer::default().announce_controlled_with_checkpoint(
                &lan,
                &[target],
                &Clip {
                    url: "http://192.0.2.250:3400/media/journal.wav".into(),
                    title: "Synthetic journal fixture".into(),
                    duration: Duration::from_millis(1),
                },
                35,
                None,
                &AnnouncementCallbacks {
                    clip_url: None,
                    authorize: &|| Ok(()),
                    cap: &|_| 100,
                    restore_cap: None,
                    completed: &|_, _| {},
                },
                &AnnouncementControl::default(),
                &capture,
            );
            assert!(
                sim.soap_log()
                    .iter()
                    .all(|entry| entry.action.starts_with("Get") || entry.action == "Browse"),
                "initial journal capture must precede every simulated mutation"
            );
            let record = capture.0.into_inner().unwrap().expect("initial checkpoint");
            assert!(record.valid());
            assert!(!record.recoverable());
            record
        })
    }
}

#[test]
fn absent_journal_stays_empty_without_publishing_recovery() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    assert!(journal.pending().unwrap().is_empty());
    assert!(journal.completed().unwrap().is_empty());
    assert!(!dir.path().join(FILE).exists());
    assert!(dir.path().join(LOCK).exists());
}

#[test]
fn full_context_and_exact_record_survive_a_new_journal_owner() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let mut ctx = context(1);
    // Human room labels may repeat; the core record binds the physical audience.
    ctx.rooms.push(ctx.rooms[0].clone());
    ctx.interrupted_sleeps
        .push(PlayerId("sleeping-room".into()));
    ctx.interrupted_sleep_deadlines
        .insert("sleeping-room".into(), 42_000);
    let record = recovery(1);
    journal.checkpoint(&ctx).save(&record).unwrap();
    let reopened = Journal::new(dir.path()).unwrap();
    let pending = reopened.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].context, ctx);
    assert_eq!(pending[0].recovery, record);
    assert!(!pending[0].claimed);
    assert!(pending[0].report.is_none());
}

#[test]
fn invalid_context_cannot_poison_a_missing_or_existing_journal() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let mut invalid = Vec::new();
    let mut c = context(2);
    c.id = "wrong-id".into();
    invalid.push(c);
    let mut c = context(2);
    c.client = " ".into();
    invalid.push(c);
    let mut c = context(2);
    c.client = "owner\nother".into();
    invalid.push(c);
    let mut c = context(2);
    c.kind = "unknown".into();
    invalid.push(c);
    let mut c = context(2);
    c.created_at = -1;
    invalid.push(c);
    let mut c = context(2);
    c.rooms.clear();
    invalid.push(c);
    let mut c = context(2);
    c.rooms[0].push('\n');
    invalid.push(c);
    let mut c = context(2);
    c.interrupted_sleeps.push(PlayerId("sleeping-room".into()));
    invalid.push(c);
    let mut c = context(2);
    c.interrupted_sleep_deadlines
        .insert("unlisted-room".into(), 1);
    invalid.push(c);
    let mut c = context(2);
    c.interrupted_sleeps.push(PlayerId("sleeping-room".into()));
    c.interrupted_sleep_deadlines
        .insert("sleeping-room".into(), -1);
    invalid.push(c);
    for context in &invalid {
        assert!(journal.checkpoint(context).save(&recovery(2)).is_err());
        assert!(!dir.path().join(FILE).exists());
    }
    journal.checkpoint(&context(1)).save(&recovery(1)).unwrap();
    let before = fs::read(dir.path().join(FILE)).unwrap();
    for context in &invalid {
        assert!(journal.checkpoint(context).save(&recovery(2)).is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    }
}

#[test]
fn a_stale_checkpoint_cannot_claim_or_complete_a_newer_revision() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let checkpoint = journal.checkpoint(&context(1));
    let old = recovery(1);
    checkpoint.save(&old).unwrap();
    let current = change_record(&old, |saved| {
        saved["clip_url"] = "http://192.0.2.250:3400/media/newer.wav".into();
    });
    checkpoint.save(&current).unwrap();
    assert!(!checkpoint.current(&old));
    assert!(!checkpoint.claim(&old).unwrap());
    assert!(checkpoint.complete(&old, &report("stale")).is_err());
    assert!(checkpoint.current(&current));
    assert!(checkpoint.claim(&current).unwrap());
    assert!(checkpoint.save(&current).is_err());
    assert!(!checkpoint.claim(&current).unwrap());
}

#[test]
fn caller_household_and_creation_identity_cannot_be_rebound() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(1);
    let record = recovery(1);
    journal.checkpoint(&ctx).save(&record).unwrap();
    let mut wrong_context = ctx.clone();
    wrong_context.client = "other-owner".into();
    assert!(journal.checkpoint(&wrong_context).save(&record).is_err());
    for different_household in [true, false] {
        let changed = change_record(&record, |saved| {
            if different_household {
                saved["household"] = "another-household".into();
                saved["before"]["id"] = "another-household".into();
            } else {
                saved["created_at"] = (record.created_at() + 1).into();
            }
        });
        assert!(journal.checkpoint(&ctx).save(&changed).is_err());
    }
    assert_eq!(journal.pending().unwrap()[0].recovery, record);
}

#[test]
fn a_claim_survives_restart_and_only_retires_without_replay() {
    let dir = directory();
    let original = Journal::new(dir.path()).unwrap();
    let ctx = context(1);
    let record = recovery(1);
    let owner = original.checkpoint(&ctx);
    owner.save(&record).unwrap();
    assert!(owner.claim(&record).unwrap());
    let claimed = original.pending().unwrap().remove(0);
    assert!(claimed.claimed);
    assert!(
        !original
            .retire_claimed(&claimed, &report("active"))
            .unwrap()
    );
    assert!(owner.current(&record));

    let same_process = original.checkpoint(&ctx);
    assert!(!same_process.current(&record));
    assert!(!same_process.claim(&record).unwrap());
    assert!(same_process.save(&record).is_err());
    assert!(
        same_process
            .complete(&record, &report("other checkpoint"))
            .is_err()
    );

    let restarted = Journal::new(dir.path()).unwrap();
    let stranger = restarted.checkpoint(&ctx);
    assert!(!stranger.current(&record));
    assert!(!stranger.claim(&record).unwrap());
    assert!(stranger.save(&record).is_err());
    assert!(stranger.complete(&record, &report("unauthorized")).is_err());
    let mut stale = claimed.clone();
    stale.context.client = "wrong-owner".into();
    assert!(!restarted.retire_claimed(&stale, &report("stale")).unwrap());
    let mut stale = claimed.clone();
    stale.recovery = change_record(&record, |saved| {
        saved["clip_url"] = "http://192.0.2.250:3400/media/stale.wav".into();
    });
    assert!(!restarted.retire_claimed(&stale, &report("stale")).unwrap());
    assert!(
        restarted
            .retire_claimed(&claimed, &report("interrupted cleanup"))
            .unwrap()
    );
    assert!(
        !restarted
            .retire_claimed(&claimed, &report("repeat"))
            .unwrap()
    );
    assert!(restarted.pending().unwrap().is_empty());
    assert_eq!(
        restarted.completed().unwrap()[0].report,
        Some(report("interrupted cleanup"))
    );
    assert!(!owner.current(&record));
    assert!(owner.complete(&record, &report("late owner")).is_err());
}

#[test]
fn an_abandoned_claim_retires_without_replaying_in_the_same_process() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(1);
    let record = recovery(1);
    let checkpoint = journal.checkpoint(&ctx);
    checkpoint.save(&record).unwrap();
    assert!(checkpoint.claim(&record).unwrap());
    let pending = journal.pending().unwrap().remove(0);
    assert!(!journal.retire_claimed(&pending, &report("live")).unwrap());
    drop(checkpoint);
    assert!(
        journal
            .retire_claimed(&pending, &report("abandoned"))
            .unwrap()
    );
    assert!(journal.pending().unwrap().is_empty());
    assert_eq!(
        journal.completed().unwrap()[0].report,
        Some(report("abandoned"))
    );
    let later = journal.checkpoint(&ctx);
    assert!(!later.claim(&record).unwrap());
    assert!(!later.current(&record));
    assert!(later.save(&record).is_err());
}

#[test]
fn finalization_requires_a_durable_claim_and_preserves_pending_evidence() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let checkpoint = journal.checkpoint(&context(1));
    let record = recovery(1);
    checkpoint.save(&record).unwrap();
    let before = fs::read(dir.path().join(FILE)).unwrap();
    assert!(checkpoint.complete(&record, &report("unclaimed")).is_err());
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    let pending = journal.pending().unwrap();
    assert_eq!(pending.len(), 1);
    assert!(!pending[0].claimed);
    assert!(pending[0].report.is_none());
    assert!(journal.completed().unwrap().is_empty());
    assert!(checkpoint.claim(&record).unwrap());
    checkpoint.complete(&record, &report("claimed")).unwrap();
    assert!(journal.pending().unwrap().is_empty());
    assert_eq!(journal.completed().unwrap().len(), 1);
}

#[test]
fn a_terminal_receipt_is_idempotent_and_cannot_reopen_delivery() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let checkpoint = journal.checkpoint(&context(1));
    let record = recovery(1);
    checkpoint.save(&record).unwrap();
    assert!(checkpoint.claim(&record).unwrap());
    let receipt = report("restored");
    checkpoint.complete(&record, &receipt).unwrap();
    let bytes = fs::read(dir.path().join(FILE)).unwrap();
    checkpoint.complete(&record, &receipt).unwrap();
    assert_eq!(bytes, fs::read(dir.path().join(FILE)).unwrap());
    assert!(checkpoint.complete(&record, &report("different")).is_err());
    assert!(checkpoint.save(&record).is_err());
    assert!(!checkpoint.claim(&record).unwrap());
    assert!(!checkpoint.current(&record));
    assert!(
        Journal::new(dir.path())
            .unwrap()
            .pending()
            .unwrap()
            .is_empty()
    );
}

#[test]
fn corrupt_ambiguous_or_oversized_files_are_preserved_without_a_new_write() {
    let seed = directory();
    Journal::new(seed.path())
        .unwrap()
        .checkpoint(&context(1))
        .save(&recovery(1))
        .unwrap();
    let valid: serde_json::Value =
        serde_json::from_slice(&fs::read(seed.path().join(FILE)).unwrap()).unwrap();
    let mut cases = vec![b"{".to_vec(), vec![b' '; 8 * 1024 * 1024 + 1]];
    let mut v = valid.clone();
    v["version"] = 2.into();
    cases.push(serde_json::to_vec(&v).unwrap());
    let mut v = valid.clone();
    v["unexpected"] = true.into();
    cases.push(serde_json::to_vec(&v).unwrap());
    let mut v = valid.clone();
    v["records"][0]["context"]["kind"] = "invalid".into();
    cases.push(serde_json::to_vec(&v).unwrap());
    let mut v = valid.clone();
    v["records"][0]["recovery"]["id"] = "".into();
    cases.push(serde_json::to_vec(&v).unwrap());
    let mut v = valid.clone();
    let duplicate = v["records"][0].clone();
    v["records"].as_array_mut().unwrap().push(duplicate);
    cases.push(serde_json::to_vec(&v).unwrap());
    for bytes in cases {
        let dir = directory();
        fs::write(dir.path().join(FILE), &bytes).unwrap();
        let journal = Journal::new(dir.path()).unwrap();
        assert!(journal.pending().is_err());
        assert!(journal.checkpoint(&context(2)).save(&recovery(2)).is_err());
        assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), bytes);
    }
}

#[test]
fn capacity_never_evicts_pending_work_and_old_receipts_yield_first() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    for n in 0..64 {
        journal.checkpoint(&context(n)).save(&recovery(n)).unwrap();
    }
    let before = fs::read(dir.path().join(FILE)).unwrap();
    assert!(
        journal
            .checkpoint(&context(64))
            .save(&recovery(64))
            .is_err()
    );
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    let first = journal.checkpoint(&context(0));
    assert!(first.claim(&recovery(0)).unwrap());
    first.complete(&recovery(0), &report("done")).unwrap();
    journal
        .checkpoint(&context(64))
        .save(&recovery(64))
        .unwrap();
    let pending = journal.pending().unwrap();
    assert_eq!(pending.len(), 64);
    assert!(!pending.iter().any(|p| p.recovery.id() == recovery(0).id()));
    assert!(pending.iter().any(|p| p.recovery.id() == recovery(64).id()));
}

#[test]
fn independent_writers_keep_every_accepted_revision() {
    let dir = directory();
    let first = Journal::new(dir.path()).unwrap();
    let second = Journal::new(dir.path()).unwrap();
    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        for (journal, start) in [(first, 0), (second, 20)] {
            let barrier = Arc::clone(&barrier);
            scope.spawn(move || {
                barrier.wait();
                for n in start..start + 20 {
                    let checkpoint = journal.checkpoint(&context(n));
                    let record = recovery(n);
                    let deadline = Instant::now() + Duration::from_secs(5);
                    while let Err(error) = checkpoint.save(&record) {
                        assert!(error.to_string().contains("another writer"), "{error}");
                        assert!(Instant::now() < deadline);
                        std::thread::yield_now();
                    }
                }
            });
        }
    });
    let records = Journal::new(dir.path()).unwrap().pending().unwrap();
    assert_eq!(records.len(), 40);
    for n in 0..40 {
        assert!(records.iter().any(|p| p.recovery == recovery(n)));
    }
}

#[test]
fn file_lock_contention_refuses_before_publication_and_fail_closes_current() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let checkpoint = journal.checkpoint(&context(1));
    let record = recovery(1);
    checkpoint.save(&record).unwrap();
    let before = fs::read(dir.path().join(FILE)).unwrap();
    let held = OpenOptions::new()
        .write(true)
        .open(dir.path().join(LOCK))
        .unwrap();
    held.lock().unwrap();
    assert!(checkpoint.save(&record).is_err());
    assert!(checkpoint.claim(&record).is_err());
    assert!(!checkpoint.current(&record));
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    drop(held);
    assert!(checkpoint.current(&record));
}

#[cfg(unix)]
#[test]
fn journal_and_lock_are_private_files() {
    use std::os::unix::fs::PermissionsExt;
    let dir = directory();
    Journal::new(dir.path())
        .unwrap()
        .checkpoint(&context(1))
        .save(&recovery(1))
        .unwrap();
    for name in [FILE, LOCK] {
        assert_eq!(
            fs::metadata(dir.path().join(name))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn explicit_abandonment_is_durable_and_old_checkpoints_cannot_restore_or_reopen() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(71);
    let record = recovery(71);
    let checkpoint = journal.checkpoint(&ctx);
    checkpoint.save(&record).unwrap();
    let pending = journal.pending().unwrap();
    let abandoned = report("recovery abandoned; current playback left as found");
    assert!(journal.abandon(&pending, std::slice::from_ref(&abandoned)).unwrap());
    assert!(!checkpoint.current(&record));
    assert!(!checkpoint.claim(&record).unwrap());
    assert!(checkpoint.save(&record).is_err());
    assert!(checkpoint.complete(&record, &report("late cleanup")).is_err());
    let reopened = Journal::new(dir.path()).unwrap();
    assert!(reopened.pending().unwrap().is_empty());
    assert_eq!(reopened.completed().unwrap()[0].report, Some(abandoned));
    assert!(!reopened.checkpoint(&ctx).claim(&record).unwrap());
    assert!(!reopened.abandon(&pending, &[report("duplicate request")]).unwrap());
}

#[test]
fn abandonment_compares_the_complete_unclaimed_job_set_before_any_retirement() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(72);
    let a = recovery(72);
    let b = recovery(73);
    let first = journal.checkpoint(&ctx);
    let second = journal.checkpoint(&ctx);
    first.save(&a).unwrap();
    let older_set = journal.pending().unwrap();
    second.save(&b).unwrap();
    let both = journal.pending().unwrap();
    let before = fs::read(dir.path().join(FILE)).unwrap();
    assert!(!journal.abandon(&older_set, &[report("stale partial set")]).unwrap());
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    assert!(second.claim(&b).unwrap());
    let claimed = fs::read(dir.path().join(FILE)).unwrap();
    assert!(!journal.abandon(&both, &[report("a"), report("b")]).unwrap());
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), claimed);
    assert_eq!(journal.pending().unwrap().len(), 2);
    let completed = report("original cleanup receipt");
    second.complete(&b, &completed).unwrap();
    let remaining = journal.pending().unwrap();
    assert_eq!(remaining.len(), 1);
    assert!(journal.abandon(&remaining, &[report("a abandoned")]).unwrap());
    let receipts = journal.completed().unwrap();
    assert_eq!(receipts.len(), 2);
    assert_eq!(receipts.iter().find(|entry| entry.recovery == b).unwrap().report, Some(completed));
}

#[test]
fn abandonment_rejects_changed_revisions_callers_and_duplicate_proofs_without_writes() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(74);
    let original = recovery(74);
    let checkpoint = journal.checkpoint(&ctx);
    checkpoint.save(&original).unwrap();
    let old = journal.pending().unwrap();
    let revised = change_record(&original, |saved| {
        saved["clip_url"] = "http://192.0.2.250:3400/media/revised.wav".into();
    });
    checkpoint.save(&revised).unwrap();
    let mut wrong_owner = journal.pending().unwrap();
    wrong_owner[0].context.client = "different-original-caller".into();
    let before = fs::read(dir.path().join(FILE)).unwrap();
    assert!(!journal.abandon(&old, &[report("stale revision")]).unwrap());
    assert!(!journal.abandon(&wrong_owner, &[report("wrong owner")]).unwrap());
    let mut wrong_proof = old.clone();
    wrong_proof[0].recovery = recovery(75);
    assert!(!journal.abandon(&wrong_proof, &[report("wrong proof")]).unwrap());
    assert!(journal.abandon(&[old[0].clone(), old[0].clone()], &[report("a"), report("b")]).is_err());
    assert!(journal.abandon(&old, &[]).is_err());
    assert_eq!(fs::read(dir.path().join(FILE)).unwrap(), before);
    assert!(checkpoint.current(&revised));
    assert!(!checkpoint.current(&original));
}

#[test]
fn claim_and_abandon_race_for_one_durable_authority() {
    for n in 80..96 {
        let dir = directory();
        let journal = Journal::new(dir.path()).unwrap();
        let record = recovery(n);
        let checkpoint = journal.checkpoint(&context(n));
        checkpoint.save(&record).unwrap();
        let pending = journal.pending().unwrap();
        let barrier = Arc::new(Barrier::new(2));
        let (claimed, abandoned) = std::thread::scope(|scope| {
            let start = Arc::clone(&barrier);
            let worker = Arc::clone(&checkpoint);
            let expected = record.clone();
            let claim = scope.spawn(move || {
                start.wait();
                worker.claim(&expected).unwrap()
            });
            let start = Arc::clone(&barrier);
            let abandoning = journal.clone();
            let abandon = scope.spawn(move || {
                start.wait();
                abandoning.abandon(&pending, &[report("explicit abandonment")]).unwrap()
            });
            (claim.join().unwrap(), abandon.join().unwrap())
        });
        assert_ne!(claimed, abandoned, "exactly one transition can win");
        let reopened = Journal::new(dir.path()).unwrap();
        if claimed {
            assert_eq!(reopened.pending().unwrap().len(), 1);
            assert!(reopened.pending().unwrap()[0].claimed);
            assert!(reopened.completed().unwrap().is_empty());
            assert!(checkpoint.current(&record));
        } else {
            assert!(reopened.pending().unwrap().is_empty());
            assert_eq!(reopened.completed().unwrap().len(), 1);
            assert!(!checkpoint.current(&record));
            assert!(!checkpoint.claim(&record).unwrap());
        }
    }
}

#[test]
fn abandonment_never_evicts_its_own_receipts_to_report_a_partial_success() {
    let dir = directory();
    let journal = Journal::new(dir.path()).unwrap();
    let ctx = context(100);
    let completed = recovery(100);
    let pending = recovery(101);
    let checkpoint = journal.checkpoint(&ctx);
    checkpoint.save(&completed).unwrap();
    checkpoint.save(&pending).unwrap();
    assert!(checkpoint.claim(&completed).unwrap());
    checkpoint.complete(&completed, &report("old")).unwrap();

    // Leave a valid ledger just below its byte bound. Completing the other
    // household would fit only by discarding this same job's older receipt.
    let path = dir.path().join(FILE);
    let bytes = fs::read(&path).unwrap();
    let mut ledger: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let padding = "x".repeat(super::MAX_BYTES - bytes.len() + "old".len() - 16);
    let record = ledger["records"].as_array_mut().unwrap().iter_mut()
        .find(|record| record["recovery"]["id"] == completed.id()).unwrap();
    record["state"]["report"] = serde_json::to_value(report(&padding)).unwrap();
    let before = serde_json::to_vec(&ledger).unwrap();
    assert!(before.len() < super::MAX_BYTES);
    fs::write(&path, &before).unwrap();
    let expected = journal.pending().unwrap();
    assert!(journal.abandon(&expected, &[report("explicitly abandoned recovery")]).is_err());
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(journal.pending().unwrap().len(), 1);
    assert_eq!(journal.completed().unwrap().len(), 1);
    assert!(checkpoint.current(&pending));
}
