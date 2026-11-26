//! Restart the surface over real simulator state; only persistence/process
//! interruptions are injected. No test reaches a physical speaker.

use super::super::{AnnounceRequest, AnnouncementJobPhase};
use super::*;
use fsonos_core::announce::Announcer;
use fsonos_core::announce::clip::MediaStore;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::AtomicUsize;
use std::time::Duration;

#[derive(Default)]
struct Stored {
    records: HashMap<String, PendingAnnouncement>,
    owners: HashMap<String, usize>,
}

struct MemoryJournal {
    stored: Arc<Mutex<Stored>>,
    owner: usize,
    crash: Arc<AtomicBool>,
    fail_save: Arc<AtomicBool>,
    fail_read: Arc<AtomicBool>,
    claims: Arc<AtomicUsize>,
}

impl MemoryJournal {
    fn new() -> Self {
        Self {
            stored: Arc::new(Mutex::new(Stored::default())),
            owner: 1,
            crash: Arc::new(AtomicBool::new(true)),
            fail_save: Arc::new(AtomicBool::new(false)),
            fail_read: Arc::new(AtomicBool::new(false)),
            claims: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn restart(&self) -> Arc<Self> {
        Arc::new(Self {
            stored: Arc::clone(&self.stored),
            owner: self.owner + 1,
            crash: Arc::new(AtomicBool::new(false)),
            fail_save: Arc::clone(&self.fail_save),
            fail_read: Arc::clone(&self.fail_read),
            claims: Arc::clone(&self.claims),
        })
    }

    fn read(&self, completed: bool) -> Result<Vec<PendingAnnouncement>, CoreError> {
        if self.fail_read.load(Ordering::Acquire) {
            return Err(CoreError::Store("injected unreadable journal".into()));
        }
        Ok(self
            .stored
            .lock()
            .unwrap()
            .records
            .values()
            .filter(|entry| entry.report.is_some() == completed)
            .cloned()
            .collect())
    }
}

struct MemoryCheckpoint {
    stored: Arc<Mutex<Stored>>,
    owner: usize,
    context: AnnouncementContext,
    crash: Arc<AtomicBool>,
    fail_save: Arc<AtomicBool>,
    claims: Arc<AtomicUsize>,
}

impl AnnouncementCheckpoint for MemoryCheckpoint {
    fn save(&self, recovery: &AnnouncementRecovery) -> Result<(), CoreError> {
        if self.fail_save.load(Ordering::Acquire) {
            return Err(CoreError::Store("injected full journal".into()));
        }
        {
            let mut stored = self.stored.lock().unwrap();
            if stored
                .records
                .get(recovery.id())
                .is_some_and(|entry| entry.claimed || entry.report.is_some())
            {
                return Err(CoreError::Store("checkpoint is already claimed".into()));
            }
            let entry = PendingAnnouncement {
                context: self.context.clone(),
                recovery: recovery.clone(),
                claimed: false,
                report: None,
            };
            assert!(entry.valid());
            // Exercise the actual persisted record format, not an Arc to the
            // old announcer or cancellation token.
            let bytes = serde_json::to_vec(&entry).unwrap();
            stored.records.insert(
                recovery.id().into(),
                serde_json::from_slice(&bytes).unwrap(),
            );
        }
        assert!(
            !(recovery.recoverable() && self.crash.swap(false, Ordering::AcqRel)),
            "injected process stop after durable clip ownership"
        );
        Ok(())
    }

    fn claim(&self, recovery: &AnnouncementRecovery) -> Result<bool, CoreError> {
        let mut stored = self.stored.lock().unwrap();
        let Some(entry) = stored.records.get_mut(recovery.id()).filter(|entry| {
            entry.recovery == *recovery && !entry.claimed && entry.report.is_none()
        }) else {
            return Ok(false);
        };
        entry.claimed = true;
        stored.owners.insert(recovery.id().into(), self.owner);
        self.claims.fetch_add(1, Ordering::AcqRel);
        Ok(true)
    }

    fn complete(
        &self,
        recovery: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError> {
        let mut stored = self.stored.lock().unwrap();
        if stored.owners.get(recovery.id()) != Some(&self.owner) {
            return Err(CoreError::Store("another worker owns completion".into()));
        }
        let entry = stored.records.get_mut(recovery.id()).unwrap();
        if entry.recovery != *recovery || !entry.claimed {
            return Err(CoreError::Store("completion identity changed".into()));
        }
        entry.report = Some(report.clone());
        Ok(())
    }

    fn current(&self, recovery: &AnnouncementRecovery) -> bool {
        let stored = self.stored.lock().unwrap();
        stored.records.get(recovery.id()).is_some_and(|entry| {
            entry.recovery == *recovery
                && entry.report.is_none()
                && (!entry.claimed || stored.owners.get(recovery.id()) == Some(&self.owner))
        })
    }
}

impl AnnouncementJournal for MemoryJournal {
    fn checkpoint(&self, context: &AnnouncementContext) -> Arc<dyn AnnouncementCheckpoint> {
        Arc::new(MemoryCheckpoint {
            stored: Arc::clone(&self.stored),
            owner: self.owner,
            context: context.clone(),
            crash: Arc::clone(&self.crash),
            fail_save: Arc::clone(&self.fail_save),
            claims: Arc::clone(&self.claims),
        })
    }

    fn pending(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        self.read(false)
    }
    fn completed(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        self.read(true)
    }

    fn records(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        if self.fail_read.load(Ordering::Acquire) {
            return Err(CoreError::Store("injected unreadable journal".into()));
        }
        Ok(self
            .stored
            .lock()
            .unwrap()
            .records
            .values()
            .cloned()
            .collect())
    }

    fn abandon(
        &self,
        pending: &[PendingAnnouncement],
        reports: &[HouseholdAnnouncement],
    ) -> Result<bool, CoreError> {
        if self.fail_save.load(Ordering::Acquire) {
            return Err(CoreError::Store("injected full journal".into()));
        }
        let Some(first) = pending.first() else { return Ok(false); };
        let mut stored = self.stored.lock().unwrap();
        let unfinished: Vec<_> = stored.records.values().filter(|entry| {
            entry.context.id == first.context.id && entry.report.is_none()
        }).collect();
        if unfinished.len() != pending.len() || pending.len() != reports.len()
            || unfinished.iter().any(|entry| entry.claimed || !pending.contains(entry))
        {
            return Ok(false);
        }
        for (entry, report) in pending.iter().zip(reports) {
            stored.records.get_mut(entry.recovery.id()).unwrap().report = Some(report.clone());
        }
        Ok(true)
    }

    fn retire_claimed(
        &self,
        pending: &PendingAnnouncement,
        report: &HouseholdAnnouncement,
    ) -> Result<bool, CoreError> {
        let mut stored = self.stored.lock().unwrap();
        if stored.owners.get(pending.recovery.id()) == Some(&self.owner) {
            return Ok(false);
        }
        let Some(entry) = stored
            .records
            .get_mut(pending.recovery.id())
            .filter(|entry| **entry == *pending && entry.claimed && entry.report.is_none())
        else {
            return Ok(false);
        };
        entry.report = Some(report.clone());
        Ok(true)
    }
}

struct ObservedLan {
    lan: SimLan,
    writes: Arc<AtomicUsize>,
    offline: Arc<AtomicBool>,
}

impl Transport for ObservedLan {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.offline.load(Ordering::Acquire) {
            return Err(ProtoError::Malformed(
                "injected unavailable household".into(),
            ));
        }
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.fetch_add(1, Ordering::AcqRel);
        }
        self.lan.soap_post(host, path, action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.lan.http_get(url)
    }
}

struct House {
    sim: SimHandle,
    household: HouseholdState,
    data: tempfile::TempDir,
    journal: Arc<MemoryJournal>,
    writes: Arc<AtomicUsize>,
    offline: Arc<AtomicBool>,
}

impl House {
    fn new(s2: bool) -> Self {
        let builder = SimHousehold::builder();
        let spec = SimPlayerSpec::new(
            "Recovery",
            if s2 {
                SimModel::One
            } else {
                SimModel::Play5Gen1
            },
        );
        let sim = if s2 {
            builder.s2([spec])
        } else {
            builder.s1([spec])
        }
        .spawn()
        .unwrap();
        let host = sim.player("Recovery").unwrap().ip;
        let mut household = HouseholdState::default();
        household.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        soap::set_av_transport_uri(
            &sim.lan(),
            host,
            "x-rincon-mp3radio://stream.example.invalid/original.mp3",
            "",
        )
        .unwrap();
        soap::play(&sim.lan(), host).unwrap();
        soap::set_volume(&sim.lan(), host, 60).unwrap();
        Self {
            sim,
            household,
            data: tempfile::tempdir().unwrap(),
            journal: Arc::new(MemoryJournal::new()),
            writes: Arc::new(AtomicUsize::new(0)),
            offline: Arc::new(AtomicBool::new(false)),
        }
    }

    fn surface(&self, journal: Arc<MemoryJournal>, policy: Policy) -> Surface {
        let household = self.household.clone();
        let mut announcements = Announcements::new(
            MediaStore::new(self.data.path()),
            Box::new(|| Some("http://192.0.2.1:3400".into())),
        )
        .with_journal(journal);
        announcements.announcer =
            Announcer::with_timing(Duration::from_millis(1), Duration::from_millis(10));
        Surface::new(
            Box::new(ObservedLan {
                lan: self.sim.lan(),
                writes: Arc::clone(&self.writes),
                offline: Arc::clone(&self.offline),
            }),
            Box::new(move |_| Ok(vec![household.clone()])),
            policy,
            Box::new(SystemClock),
        )
        .with_announcements(announcements)
    }

    fn crash(&self) -> PendingAnnouncement {
        let surface = self.surface(Arc::clone(&self.journal), Policy::default());
        let request = AnnounceRequest {
            chime: Some("bell".into()),
            rooms: vec!["Recovery".into()],
            ..AnnounceRequest::default()
        };
        assert!(
            catch_unwind(AssertUnwindSafe(
                || surface.announce(&Client::McpStdio, &request)
            ))
            .is_err()
        );
        let records = self.journal.pending().unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].recovery.recoverable());
        records[0].clone()
    }

    fn host(&self) -> IpAddr {
        self.sim.player("Recovery").unwrap().ip
    }
}

#[test]
fn restart_restores_owned_music_with_current_original_caller_caps_and_retains_receipt() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let entry = h.crash();
        let journal = h.journal.restart();
        let policy = Policy::from_toml(
            "[defaults]\nmax_volume = 17\n[clients.\"mcp-stdio\"]\ncapped = true\n",
        )
        .unwrap();
        let restarted = h.surface(Arc::clone(&journal), policy);
        assert_eq!(
            restarted.recover_announcements().unwrap(),
            AnnouncementRecoveryPass {
                finished: 1,
                pending: 0
            }
        );
        assert_eq!(
            soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri,
            "x-rincon-mp3radio://stream.example.invalid/original.mp3"
        );
        assert_eq!(
            soap::get_transport_info(&h.sim.lan(), h.host())
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&h.sim.lan(), h.host()).unwrap(), 17);
        let receipt = restarted
            .get_announcement(&Client::McpStdio, &entry.context.id)
            .unwrap();
        assert_eq!(
            receipt.phase,
            AnnouncementJobPhase::Failed,
            "interrupted delivery must not be reported as completed playback"
        );
        assert!(!receipt.can_cancel);
        assert!(receipt.recovery);
        assert!(
            receipt.result.as_ref().unwrap().households[0]
                .not_restored
                .is_empty(),
            "{receipt:?}"
        );
        assert!(
            restarted
                .list_announcements(&Client::LoopbackHttp)
                .unwrap()
                .is_empty()
        );
        let writes = h.writes.load(Ordering::Acquire);
        let another = h.surface(journal.restart(), Policy::default());
        another.prepare_announcement_recovery().unwrap();
        assert_eq!(
            another
                .get_announcement(&Client::McpStdio, &entry.context.id)
                .unwrap()
                .result,
            receipt.result
        );
        assert_eq!(
            another.recover_announcements().unwrap(),
            AnnouncementRecoveryPass::default()
        );
        assert_eq!(h.writes.load(Ordering::Acquire), writes);
    }
}

#[test]
fn paused_clip_and_new_listener_source_are_preserved_without_a_single_write() {
    for replace in [false, true] {
        let h = House::new(true);
        let entry = h.crash();
        if replace {
            soap::set_av_transport_uri(
                &h.sim.lan(),
                h.host(),
                "http://stream.example.invalid/new.mp3",
                "",
            )
            .unwrap();
            soap::play(&h.sim.lan(), h.host()).unwrap();
        } else {
            soap::pause(&h.sim.lan(), h.host()).unwrap();
        }
        let before = soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri;
        let writes = h.writes.load(Ordering::Acquire);
        let surface = h.surface(h.journal.restart(), Policy::default());
        assert_eq!(surface.recover_announcements().unwrap().pending, 0);
        assert_eq!(h.writes.load(Ordering::Acquire), writes);
        assert_eq!(
            soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri,
            before
        );
        assert!(
            !surface
                .get_announcement(&Client::McpStdio, &entry.context.id)
                .unwrap()
                .result
                .unwrap()
                .households[0]
                .not_restored
                .is_empty()
        );
    }
}

#[test]
fn offline_household_remains_visible_and_pending_until_fresh_evidence_arrives() {
    let h = House::new(false);
    let entry = h.crash();
    h.offline.store(true, Ordering::Release);
    let journal = h.journal.restart();
    let surface = h.surface(Arc::clone(&journal), Policy::default());
    let writes = h.writes.load(Ordering::Acquire);
    assert_eq!(
        surface.recover_announcements().unwrap(),
        AnnouncementRecoveryPass {
            finished: 0,
            pending: 1
        }
    );
    assert_eq!(h.writes.load(Ordering::Acquire), writes);
    let job = surface
        .get_announcement(&Client::McpStdio, &entry.context.id)
        .unwrap();
    assert_eq!(job.phase, AnnouncementJobPhase::Restoring);
    assert!(!job.can_cancel);
    assert!(job.done.contains("injected unavailable household"));
    assert_eq!(journal.claims.load(Ordering::Acquire), 0);
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        ..AnnounceRequest::default()
    };
    assert_eq!(
        surface
            .announce(&Client::McpStdio, &request)
            .unwrap_err()
            .code,
        ErrorCode::NotReady
    );
    h.offline.store(false, Ordering::Release);
    assert_eq!(
        surface.recover_announcements().unwrap(),
        AnnouncementRecoveryPass {
            finished: 1,
            pending: 0
        }
    );
}

#[test]
fn orphaned_cleanup_claim_is_retired_without_replaying_any_uncertain_write() {
    let h = House::new(true);
    let entry = h.crash();
    assert!(
        h.journal
            .checkpoint(&entry.context)
            .claim(&entry.recovery)
            .unwrap()
    );
    let writes = h.writes.load(Ordering::Acquire);
    let surface = h.surface(h.journal.restart(), Policy::default());
    assert_eq!(
        surface.recover_announcements().unwrap(),
        AnnouncementRecoveryPass {
            finished: 1,
            pending: 0
        }
    );
    assert_eq!(h.writes.load(Ordering::Acquire), writes);
    let receipt = surface
        .get_announcement(&Client::McpStdio, &entry.context.id)
        .unwrap();
    assert!(receipt.done.contains("uncertain writes were not replayed"));
    assert_eq!(
        surface
            .cancel_announcement(&Client::McpStdio, &entry.context.id)
            .unwrap(),
        receipt
    );
}

#[test]
fn unreadable_or_unwritable_journal_refuses_admission_before_speaker_changes() {
    for read in [false, true] {
        let h = House::new(false);
        if read {
            h.journal.fail_read.store(true, Ordering::Release);
        } else {
            h.journal.fail_save.store(true, Ordering::Release);
        }
        let surface = h.surface(Arc::clone(&h.journal), Policy::default());
        let request = AnnounceRequest {
            chime: Some("bell".into()),
            ..AnnounceRequest::default()
        };
        let result = surface.announce(&Client::McpStdio, &request);
        assert!(result.is_err() || result.is_ok_and(|report| !report.clean));
        assert_eq!(h.writes.load(Ordering::Acquire), 0);
        assert_eq!(soap::get_volume(&h.sim.lan(), h.host()).unwrap(), 60);
    }
}

#[test]
fn shutdown_refuses_new_recovery_and_does_not_claim_its_record() {
    let h = House::new(true);
    h.crash();
    let journal = h.journal.restart();
    let surface = h.surface(Arc::clone(&journal), Policy::default());
    surface.prepare_announcement_recovery().unwrap();
    surface.stop_announcements();
    let writes = h.writes.load(Ordering::Acquire);
    assert_eq!(
        surface.recover_announcements().unwrap_err().code,
        ErrorCode::NotReady
    );
    assert_eq!(journal.claims.load(Ordering::Acquire), 0);
    assert_eq!(surface.announcements_pending(), 0);
    assert_eq!(h.writes.load(Ordering::Acquire), writes);
}

#[test]
fn context_requires_exact_sleep_deadline_identity_and_never_stores_speech() {
    let mut context = AnnouncementContext {
        id: "0123456789abcdef0123456789abcdef".into(),
        client: "mcp-stdio".into(),
        kind: "speech".into(),
        rooms: vec!["Recovery@S2".into()],
        created_at: 1,
        interrupted_sleeps: Vec::new(),
        interrupted_sleep_deadlines: BTreeMap::new(),
    };
    assert!(context.valid());
    context
        .interrupted_sleeps
        .push(PlayerId("RINCON_TEST".into()));
    assert!(!context.valid());
    context
        .interrupted_sleep_deadlines
        .insert("RINCON_TEST".into(), 1_000);
    assert!(context.valid());
    context
        .interrupted_sleep_deadlines
        .insert("RINCON_OTHER".into(), 1_000);
    assert!(!context.valid());
    assert!(
        serde_json::to_value(&context)
            .unwrap()
            .get("text")
            .is_none()
    );
}

#[test]
fn separate_household_results_keep_the_original_job_and_its_partial_progress() {
    let first = House::new(false);
    let second = House::new(true);
    let mut completed = first.crash();
    let mut pending = second.crash();
    let mut context = completed.context.clone();
    context.rooms.extend(pending.context.rooms.clone());
    completed.context = context.clone();
    pending.context = context.clone();
    let mut report = waiting(&completed, "already finished");
    report.outcome = Ok(super::super::Ending::Finished);
    completed.claimed = true;
    completed.report = Some(report);
    let journal = first.journal.restart();
    {
        let mut stored = journal.stored.lock().unwrap();
        stored
            .records
            .insert(completed.recovery.id().into(), completed);
        stored.records.insert(pending.recovery.id().into(), pending);
    }
    let surface = first.surface(journal, Policy::default());
    surface.prepare_announcement_recovery().unwrap();
    let receipt = surface
        .get_announcement(&Client::McpStdio, &context.id)
        .unwrap();
    assert_eq!(receipt.phase, AnnouncementJobPhase::Restoring);
    assert_eq!(receipt.rooms, context.rooms);
    assert_eq!(receipt.progress.len(), 2);
    assert_eq!(
        receipt
            .progress
            .iter()
            .filter(|row| row.phase == "complete")
            .count(),
        1
    );
    assert_eq!(
        receipt
            .progress
            .iter()
            .filter(|row| row.phase == "restoring")
            .count(),
        1
    );
    assert_eq!(receipt.result.unwrap().households.len(), 2);
    assert!(!receipt.can_cancel);
}

#[test]
fn a_lost_household_receipt_cannot_turn_a_partial_job_into_success() {
    for outcome in [
        super::super::Ending::Finished,
        super::super::Ending::Cancelled,
    ] {
        let h = House::new(true);
        let mut entry = h.crash();
        entry.context.rooms.push("Another room@S1".into());
        let mut report = waiting(&entry, "finished");
        report.outcome = Ok(outcome);
        entry.claimed = true;
        entry.report = Some(report);
        h.journal
            .stored
            .lock()
            .unwrap()
            .records
            .insert(entry.recovery.id().into(), entry.clone());
        let surface = h.surface(h.journal.restart(), Policy::default());
        surface.prepare_announcement_recovery().unwrap();
        let receipt = surface
            .get_announcement(&Client::McpStdio, &entry.context.id)
            .unwrap();
        assert_eq!(receipt.phase, AnnouncementJobPhase::Failed);
        assert!(!receipt.result.unwrap().clean);
        assert!(receipt.done.contains("outcome is unknown"));
    }
}

#[test]
fn a_previously_admitted_clip_cannot_checkpoint_over_an_orphan_in_its_household() {
    let h = House::new(true);
    let old = h.crash();
    let surface = h.surface(h.journal.restart(), Policy::default());
    let mut next = serde_json::to_value(&old.recovery).unwrap();
    next["id"] = serde_json::Value::String("another-queued-household-record".into());
    let next: AnnouncementRecovery = serde_json::from_value(next).unwrap();
    assert!(next.valid());
    let context = AnnouncementContext {
        id: "00112233445566778899aabbccddeeff".into(),
        ..old.context
    };
    let with = surface.announcements.as_ref().unwrap();
    let recording = Recording::new(&**with.journal.as_ref().unwrap(), &context, &surface);
    let writes = h.writes.load(Ordering::Acquire);
    assert!(
        recording
            .save(&next)
            .unwrap_err()
            .to_string()
            .contains("still being inspected")
    );
    assert_eq!(h.writes.load(Ordering::Acquire), writes);
    assert_eq!(h.journal.pending().unwrap().len(), 1);
}

#[test]
fn a_synchronous_worker_panic_releases_live_authority_for_the_same_process_recovery() {
    let h = House::new(true);
    let surface = h.surface(Arc::clone(&h.journal), Policy::default());
    let request = AnnounceRequest {
        chime: Some("bell".into()),
        ..AnnounceRequest::default()
    };
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            surface.announce(&Client::McpStdio, &request)
        }))
        .is_err()
    );
    let jobs = surface.list_announcements(&Client::McpStdio).unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].phase, AnnouncementJobPhase::Failed);
    assert!(!jobs[0].can_cancel);
    assert!(
        !surface
            .announcements
            .as_ref()
            .unwrap()
            .jobs
            .live(&jobs[0].id)
    );
    assert_eq!(surface.announcements_pending(), 0);
    assert_eq!(
        surface.recover_announcements().unwrap(),
        AnnouncementRecoveryPass {
            finished: 1,
            pending: 0
        }
    );
    assert_eq!(
        soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri,
        "x-rincon-mp3radio://stream.example.invalid/original.mp3"
    );
}

fn confirm_abandon() -> AbandonAnnouncementRecoveryRequest {
    AbandonAnnouncementRecoveryRequest { confirm_abandon: true }
}

#[test]
fn abandonment_keeps_current_playback_and_unblocks_the_household_across_restarts() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let entry = h.crash();
        let journal = h.journal.restart();
        let surface = h.surface(Arc::clone(&journal), Policy::default());
        h.offline.store(true, Ordering::Release);
        surface.recover_announcements().unwrap();
        assert!(surface.announcement_recovery_ready(&[&h.household]).is_err());
        let before = surface.get_announcement(&Client::McpStdio, &entry.context.id).unwrap();
        assert!(before.can_abandon_recovery);
        assert!(!before.can_cancel);
        let writes = h.writes.load(Ordering::Acquire);
        let source = soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri;
        let receipt = surface.abandon_announcement_recovery(
            &Client::McpStdio, &entry.context.id, &confirm_abandon(),
        ).unwrap();
        assert_eq!(receipt.phase, AnnouncementJobPhase::Failed);
        assert!(receipt.recovery && !receipt.can_abandon_recovery && !receipt.can_cancel);
        assert!(!receipt.result.as_ref().unwrap().clean);
        assert!(receipt.done.contains("recovery abandoned by explicit request"));
        assert!(receipt.done.contains("previous playback, grouping, position and levels were not restored"));
        assert!(journal.pending().unwrap().is_empty());
        assert_eq!(journal.claims.load(Ordering::Acquire), 0);
        surface.announcement_recovery_ready(&[&h.household]).unwrap();
        let checkpoint = journal.checkpoint(&entry.context);
        assert!(!checkpoint.current(&entry.recovery));
        assert!(!checkpoint.claim(&entry.recovery).unwrap());
        assert!(checkpoint.save(&entry.recovery).is_err());
        assert_eq!(surface.abandon_announcement_recovery(
            &Client::McpStdio, &entry.context.id, &confirm_abandon(),
        ).unwrap(), receipt);
        h.offline.store(false, Ordering::Release);
        let another = h.surface(journal.restart(), Policy::default());
        another.prepare_announcement_recovery().unwrap();
        assert_eq!(another.get_announcement(&Client::McpStdio, &entry.context.id)
            .unwrap().result, receipt.result);
        assert_eq!(another.recover_announcements().unwrap(), AnnouncementRecoveryPass::default());
        assert_eq!(h.writes.load(Ordering::Acquire), writes);
        assert_eq!(soap::get_media_info(&h.sim.lan(), h.host()).unwrap().uri, source);
    }
}

#[test]
fn abandonment_requires_confirmation_and_original_caller_or_configured_owner() {
    for actor in [Client::McpStdio, Client::Cli, Client::Tailnet("OWNER@example.com".into())] {
        let h = House::new(false);
        let entry = h.crash();
        let journal = h.journal.restart();
        let surface = h.surface(Arc::clone(&journal),
            Policy::default().with_owner(Some("owner@example.com".into())));
        surface.prepare_announcement_recovery().unwrap();
        let writes = h.writes.load(Ordering::Acquire);
        for foreign in [Client::LoopbackHttp, Client::Tailnet("visitor@example.com".into())] {
            assert_eq!(surface.abandon_announcement_recovery(
                &foreign, &entry.context.id, &confirm_abandon(),
            ).unwrap_err().code, ErrorCode::NoMatch);
        }
        assert_eq!(surface.abandon_announcement_recovery(
            &actor, &entry.context.id,
            &AbandonAnnouncementRecoveryRequest { confirm_abandon: false },
        ).unwrap_err().code, ErrorCode::InvalidArgument);
        assert_eq!(journal.pending().unwrap().len(), 1);
        surface.abandon_announcement_recovery(&actor, &entry.context.id, &confirm_abandon()).unwrap();
        assert!(journal.pending().unwrap().is_empty());
        assert_eq!(h.writes.load(Ordering::Acquire), writes);
    }
}

#[test]
fn denied_or_unwritable_abandonment_retains_every_record_and_household_block() {
    for deny in [false, true] {
        let h = House::new(false);
        let entry = h.crash();
        let journal = h.journal.restart();
        let policy = if deny {
            Policy::from_toml("[clients.mcp-stdio]\ndeny = [\"announce\"]\n").unwrap()
        } else { Policy::default() };
        let surface = h.surface(Arc::clone(&journal), policy);
        surface.prepare_announcement_recovery().unwrap();
        journal.fail_save.store(!deny, Ordering::Release);
        let writes = h.writes.load(Ordering::Acquire);
        let result = surface.abandon_announcement_recovery(
            &Client::McpStdio, &entry.context.id, &confirm_abandon(),
        ).unwrap_err();
        assert_eq!(result.code, if deny { ErrorCode::PolicyDenied } else { ErrorCode::NotReady });
        assert_eq!(journal.pending().unwrap().len(), 1);
        assert!(surface.announcement_recovery_ready(&[&h.household]).is_err());
        assert_eq!(h.writes.load(Ordering::Acquire), writes);
    }
}

#[test]
fn abandonment_refuses_a_claimed_household_without_partially_retiring_the_job() {
    let first = House::new(false);
    let second = House::new(true);
    let mut a = first.crash();
    let mut b = second.crash();
    let mut context = a.context.clone();
    context.rooms.extend(b.context.rooms.clone());
    a.context = context.clone();
    b.context = context.clone();
    let journal = first.journal.restart();
    {
        let mut stored = journal.stored.lock().unwrap();
        stored.records.clear();
        stored.records.insert(a.recovery.id().into(), a.clone());
        stored.records.insert(b.recovery.id().into(), b.clone());
    }
    let surface = first.surface(Arc::clone(&journal), Policy::default());
    surface.prepare_announcement_recovery().unwrap();
    // The advisory UI receipt was unclaimed; a worker wins one claim before
    // the explicit request arrives. The other household must stay pending.
    assert!(surface.get_announcement(&Client::McpStdio, &context.id)
        .unwrap().can_abandon_recovery);
    let checkpoint = journal.checkpoint(&context);
    assert!(checkpoint.claim(&b.recovery).unwrap());
    let writes = first.writes.load(Ordering::Acquire);
    let result = surface.abandon_announcement_recovery(
        &Client::McpStdio, &context.id, &confirm_abandon(),
    ).unwrap_err();
    assert_eq!(result.code, ErrorCode::NotReady);
    assert!(result.detail.contains("already claimed"));
    assert_eq!(journal.pending().unwrap().len(), 2);
    assert!(journal.completed().unwrap().is_empty());
    assert_eq!(first.writes.load(Ordering::Acquire), writes);
    let completed = waiting(&b, "original cleanup finished with an incomplete receipt");
    checkpoint.complete(&b.recovery, &completed).unwrap();
    let receipt = surface.abandon_announcement_recovery(
        &Client::McpStdio, &context.id, &confirm_abandon(),
    ).unwrap();
    assert_eq!(receipt.phase, AnnouncementJobPhase::Failed);
    assert_eq!(receipt.result.unwrap().households.len(), 2);
    assert!(journal.pending().unwrap().is_empty());
    assert_eq!(journal.completed().unwrap().iter()
        .find(|entry| entry.recovery.id() == b.recovery.id()).unwrap().report, Some(completed));
}

#[test]
fn abandonment_request_requires_a_boolean_confirmation_and_no_extra_intent() {
    for value in [
        serde_json::json!({}),
        serde_json::json!({"confirm_abandon": "yes"}),
        serde_json::json!({"confirm_abandon": true, "restore": true}),
    ] {
        assert!(serde_json::from_value::<AbandonAnnouncementRecoveryRequest>(value).is_err());
    }
    let request: AbandonAnnouncementRecoveryRequest = serde_json::from_value(
        serde_json::json!({"confirm_abandon": true}),
    ).unwrap();
    assert!(request.confirm_abandon);
}
