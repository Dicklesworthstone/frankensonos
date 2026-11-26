//! Durable announcement recovery keeps caller policy and visible receipts at
//! the surface boundary. Speaker ownership remains the core announcer's job.

use super::{
    AnnounceDto, AnnounceReport, AnnouncementJobDto, AnnouncementProgressDto, Announcements,
    HouseholdAnnouncement, Surface,
};
use crate::{ErrorCode, Failure};
use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::announce::{AnnouncementCheckpoint, AnnouncementRecovery};
use fsonos_core::policy::Client;
use fsonos_core::sleep::{SleepResume, SleepTimer};
use fsonos_core::{CoreError, HouseholdState, Room};
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Caller and job identity retained alongside each household's core proof.
/// Speech text and uploaded audio are not copied into this record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnnouncementContext {
    pub id: String,
    pub client: String,
    pub kind: String,
    pub rooms: Vec<String>,
    pub created_at: i64,
    /// Process-local sleep continuations cannot grant a new timer after a crash.
    #[serde(default)]
    pub interrupted_sleeps: Vec<PlayerId>,
    /// The original deadline, in Unix milliseconds, fences retirement against
    /// a newly armed sleep on the same coordinator after another restart.
    #[serde(default)]
    pub interrupted_sleep_deadlines: BTreeMap<String, i64>,
}

impl AnnouncementContext {
    /// Validate private persisted input before it can select a caller policy.
    #[must_use]
    pub fn valid(&self) -> bool {
        let text = |value: &str| {
            !value.trim().is_empty() && value.len() <= 4096 && !value.chars().any(char::is_control)
        };
        super::jobs::validate_id(&self.id).is_ok()
            && text(&self.client)
            && matches!(self.kind.as_str(), "speech" | "chime" | "wav")
            && self.created_at >= 0
            && !self.rooms.is_empty()
            && self.rooms.len() <= 512
            && self.rooms.iter().all(|room| text(room))
            && self.interrupted_sleeps.len() <= 512
            && self.interrupted_sleeps.iter().all(|player| text(&player.0))
            && self.interrupted_sleeps.iter().collect::<HashSet<_>>().len()
                == self.interrupted_sleeps.len()
            && self.interrupted_sleep_deadlines.len() == self.interrupted_sleeps.len()
            && self.interrupted_sleeps.iter().all(|player| {
                self.interrupted_sleep_deadlines
                    .get(&player.0)
                    .is_some_and(|ends| *ends >= 0)
            })
    }
}

/// One household's exact durable proof, with an optional terminal receipt.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingAnnouncement {
    pub context: AnnouncementContext,
    pub recovery: AnnouncementRecovery,
    pub claimed: bool,
    pub report: Option<HouseholdAnnouncement>,
}

impl PendingAnnouncement {
    #[must_use]
    pub fn valid(&self) -> bool {
        self.context.valid() && self.recovery.valid()
    }
}

/// Durable storage is supplied by the daemon. A checkpoint is private to its
/// invocation; its first `save` must persist before any speaker write.
pub trait AnnouncementJournal: Send + Sync {
    fn checkpoint(&self, context: &AnnouncementContext) -> Arc<dyn AnnouncementCheckpoint>;

    /// Unfinished records only, including claims interrupted by a prior process.
    fn pending(&self) -> Result<Vec<PendingAnnouncement>, CoreError>;

    /// Bounded terminal receipts, to make recovery visible after another restart.
    fn completed(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        Ok(Vec::new())
    }

    /// One consistent ledger observation. File-backed implementations override
    /// this under their transaction lock, so live completion cannot appear in
    /// both the pending and completed sets during an import.
    fn records(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        let mut records = self.pending()?;
        records.extend(self.completed()?);
        Ok(records)
    }

    /// Retire every unfinished household record for one original job in one
    /// durable transaction, without speaker I/O. All entries must be exact,
    /// unclaimed pending revisions and account for the entire unfinished set.
    /// A concurrent claim, changed revision or additional pending record must
    /// return false without retiring any record. After success, `claim`,
    /// `current` and `save` must never revive these records.
    fn abandon(
        &self,
        pending: &[PendingAnnouncement],
        reports: &[HouseholdAnnouncement],
    ) -> Result<bool, CoreError>;

    /// Atomically retire an exact orphaned claim without repeating cleanup.
    /// A live checkpoint owner must prevent retirement and return false.
    fn retire_claimed(
        &self,
        pending: &PendingAnnouncement,
        report: &HouseholdAnnouncement,
    ) -> Result<bool, CoreError>;
}

/// Explicitly leave current playback as found and retire future restoration.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AbandonAnnouncementRecoveryRequest {
    /// Must be true: no previous source, grouping, position or level is restored.
    pub confirm_abandon: bool,
}

/// A bounded read/recovery pass. Pending records require another observation;
/// a completed or uncertain cleanup never grants another restoration attempt.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnnouncementRecoveryPass {
    pub finished: usize,
    pub pending: usize,
}

pub(super) struct Recording<'a> {
    journal: &'a dyn AnnouncementJournal,
    context: &'a AnnouncementContext,
    surface: &'a Surface,
    records: Mutex<HashMap<String, Recorded>>,
    refused: Mutex<HashMap<PlayerId, String>>,
}

struct Recorded {
    checkpoint: Arc<dyn AnnouncementCheckpoint>,
    sleeping: Vec<SleepTimer>,
}

impl<'a> Recording<'a> {
    pub(super) fn new(
        journal: &'a dyn AnnouncementJournal,
        context: &'a AnnouncementContext,
        surface: &'a Surface,
    ) -> Self {
        Self {
            journal,
            context,
            surface,
            records: Mutex::new(HashMap::new()),
            refused: Mutex::new(HashMap::new()),
        }
    }

    fn checkpoint(&self, recovery: &AnnouncementRecovery) -> Arc<dyn AnnouncementCheckpoint> {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        let record = records.entry(recovery.id().to_owned()).or_insert_with(|| {
            let mut context = self.context.clone();
            let sleeping = self
                .surface
                .announcement_sleep_candidates(recovery.household_state(), recovery.selected());
            context.interrupted_sleeps = sleeping
                .iter()
                .map(|timer| timer.coordinator.clone())
                .collect();
            context.interrupted_sleeps.sort_by(|a, b| a.0.cmp(&b.0));
            context.interrupted_sleeps.dedup();
            context.interrupted_sleep_deadlines = sleeping
                .iter()
                .map(|timer| (timer.coordinator.0.clone(), timer.ends.timestamp_millis()))
                .collect();
            Recorded {
                checkpoint: self.journal.checkpoint(&context),
                sleeping,
            }
        });
        Arc::clone(&record.checkpoint)
    }

    pub(super) fn announcing(&self, room: &Room) -> Option<SleepResume> {
        let expected = self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .flat_map(|record| &record.sleeping)
            .find(|timer| timer.coordinator == room.coordinator)
            .cloned();
        match self.surface.announcing_matching(room, expected.as_ref()) {
            Ok(receipt) => receipt,
            Err(error) => {
                self.refused
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .insert(room.coordinator.clone(), error.to_string());
                None
            }
        }
    }
}

impl AnnouncementCheckpoint for Recording<'_> {
    fn save(&self, recovery: &AnnouncementRecovery) -> Result<(), CoreError> {
        if let Some((_, error)) = self
            .refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .find(|(player, _)| recovery.household_state().player(player).is_some())
        {
            return Err(CoreError::Store(error.clone()));
        }
        if !self
            .records
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(recovery.id())
        {
            // A clip may have been admitted while the preceding live job was
            // still running. Recheck under the core household turn so a newly
            // orphaned checkpoint cannot be overtaken by that queued clip.
            self.surface
                .announcement_recovery_ready(&[recovery.household_state()])
                .map_err(|error| CoreError::Store(error.detail))?;
        }
        self.checkpoint(recovery).save(recovery)
    }

    fn claim(&self, recovery: &AnnouncementRecovery) -> Result<bool, CoreError> {
        self.checkpoint(recovery).claim(recovery)
    }

    fn complete(
        &self,
        recovery: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError> {
        self.checkpoint(recovery).complete(recovery, report)
    }

    fn current(&self, recovery: &AnnouncementRecovery) -> bool {
        self.checkpoint(recovery).current(recovery)
    }
}

impl Announcements {
    /// Keep recovery proofs in the daemon's private durable journal. Direct
    /// clients that do not install a journal keep their existing behavior.
    #[must_use]
    pub fn with_journal(mut self, journal: Arc<dyn AnnouncementJournal>) -> Self {
        self.journal = Some(journal);
        self
    }
}

fn failure(error: impl std::fmt::Display) -> Failure {
    Failure::new(
        ErrorCode::NotReady,
        format!("announcement recovery journal is not ready: {error}"),
    )
    .with_hint("Inspect the daemon's announcement recovery status and private journal before another announcement; nothing was replayed.")
}

fn records(journal: &dyn AnnouncementJournal) -> Result<Vec<PendingAnnouncement>, Failure> {
    let pending = journal.pending().map_err(failure)?;
    if pending.len() > 64
        || pending
            .iter()
            .any(|entry| !entry.valid() || entry.report.is_some())
    {
        return Err(failure(
            "unfinished records are invalid or exceed the bound",
        ));
    }
    Ok(pending)
}

fn same_household(record: &AnnouncementRecovery, household: &HouseholdState) -> bool {
    match (&record.household_state().id, &household.id) {
        (Some(before), Some(now)) => before == now,
        _ => household.player(record.lead()).is_some(),
    }
}

impl Surface {
    /// Load and validate startup evidence before the scheduler or listeners
    /// start. Retire only the exact old sleep fades an interrupted clip may
    /// have suspended; this performs no speaker I/O and creates no timer.
    /// It runs once, so later user-armed timers keep their own authority.
    pub fn prepare_announcement_recovery(&self) -> Result<(), Failure> {
        let Some(with) = &self.announcements else {
            return Ok(());
        };
        let Some(journal) = &with.journal else {
            return Ok(());
        };
        let mut prepared = with
            .recovery_prepared
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if *prepared {
            return Ok(());
        }
        let all = all_records(&**journal)?;
        let interrupted: Vec<_> = all
            .iter()
            .filter(|entry| entry.report.is_none() && !with.jobs.live(&entry.context.id))
            .flat_map(|entry| {
                entry
                    .context
                    .interrupted_sleep_deadlines
                    .iter()
                    .map(|(id, ends)| (PlayerId(id.clone()), *ends))
            })
            .collect();
        self.retire_announcement_sleep_candidates(&interrupted)?;
        import(self, with, &all)?;
        *prepared = true;
        Ok(())
    }

    /// One supervised recovery pass. Unavailable households remain pending;
    /// fresh listener changes are preserved and a claimed cleanup is never
    /// replayed. Reports use the original job ID and initiating caller.
    pub fn recover_announcements(&self) -> Result<AnnouncementRecoveryPass, Failure> {
        let Some(with) = &self.announcements else {
            return Ok(AnnouncementRecoveryPass::default());
        };
        let Some(journal) = &with.journal else {
            return Ok(AnnouncementRecoveryPass::default());
        };
        self.prepare_announcement_recovery()?;
        let admission = with.admitting()?;
        if with
            .recovering
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "announcement recovery is already running",
            ));
        }
        let _running = Running(&with.recovering);
        drop(admission);
        let before = all_records(&**journal)?;
        import(self, with, &before)?;
        let pending: Vec<_> = before
            .iter()
            .filter(|entry| entry.report.is_none() && !with.jobs.live(&entry.context.id))
            .collect();
        for entry in &pending {
            if *with.stopping.lock().unwrap_or_else(PoisonError::into_inner) {
                break;
            }
            if entry.claimed {
                let mut report = with.recovery_observations.lock().unwrap_or_else(PoisonError::into_inner)
                    .get(entry.recovery.id()).cloned().unwrap_or_else(|| waiting(entry,
                        "restoration was already claimed; its uncertain writes were not replayed"));
                report.restore_failed.push((entry.recovery.lead().clone(),
                    "interrupted cleanup has no remaining restoration authority; inspect this household".into()));
                sleep_notes(&entry.context, &mut report);
                journal.retire_claimed(entry, &report).map_err(failure)?;
                continue;
            }
            let caller = super::super::schedules::client_of(&entry.context.client);
            let cap = |room: &Room| {
                self.policy()
                    .max_volume(&room.name, &caller, self.clock.now())
            };
            let checkpoint = RestartCheckpoint {
                inner: journal.checkpoint(&entry.context),
                with,
                context: &entry.context,
            };
            // Core reads fresh identity/group/source/boot evidence under its
            // household turn, claims durably once, then rechecks before writes.
            let report =
                with.announcer
                    .recover(&*self.transport, &entry.recovery, &checkpoint, Some(&cap));
            with.recovery_observations
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(entry.recovery.id().to_owned(), report);
        }
        let after = all_records(&**journal)?;
        import(self, with, &after)?;
        let still_pending: HashSet<_> = after
            .iter()
            .filter(|entry| entry.report.is_none() && !with.jobs.live(&entry.context.id))
            .map(|entry| entry.recovery.id())
            .collect();
        with.recovery_observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain(|id, _| still_pending.contains(id.as_str()));
        Ok(AnnouncementRecoveryPass {
            finished: pending
                .iter()
                .filter(|entry| !still_pending.contains(entry.recovery.id()))
                .count(),
            pending: still_pending.len(),
        })
    }

    /// Abandon only retained, unclaimed recovery; ordinary cancellation keeps
    /// its ownership-checked restoration semantics. The journal transaction
    /// races the worker's durable claim, so only one can acquire authority.
    pub fn abandon_announcement_recovery(
        &self,
        client: &Client,
        id: &str,
        request: &AbandonAnnouncementRecoveryRequest,
    ) -> Result<AnnouncementJobDto, Failure> {
        self.guard(client)
            .authorize("abandon_announcement_recovery", false)?;
        super::jobs::validate_id(id)?;
        if !request.confirm_abandon {
            return Err(Failure::invalid(
                "confirm_abandon must be true to keep current playback and stop recovery",
            )
            .with_hint("Abandoning does not stop the clip or restore previous playback, groups, position or levels. Inspect the rooms and confirm only if you want to manage them yourself."));
        }
        self.prepare_announcement_recovery()?;
        let job = self.announcement_job(client, id)?.view();
        if !job.recovery {
            return Err(Failure::invalid(
                "only an interrupted announcement recovery can be abandoned",
            )
            .with_hint("Use cancel_announcement for a queued or active clip; cancellation continues safe restoration."));
        }
        let with = self
            .announcements
            .as_ref()
            .expect("a retained job has announcements");
        let journal = with.journal.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotImplemented,
                "no durable announcement journal is configured",
            )
        })?;
        let _admission = with.admitting()?;
        let all = all_records(&**journal)?;
        if !all.iter().any(|entry| {
            entry.context.id == id
                && (entry.context.client == client.key() || self.announcement_owner(client))
        }) {
            return Err(Failure::new(
                ErrorCode::NoMatch,
                "the durable announcement recovery receipt is no longer retained",
            ));
        }
        let pending: Vec<_> = all
            .iter()
            .filter(|entry| entry.context.id == id && entry.report.is_none())
            .cloned()
            .collect();
        // A repeat after durable completion is a read, even if the first
        // response was lost. Never re-open a completed household's recovery.
        if pending.is_empty() {
            import(self, with, &all)?;
            return self.announcement_job(client, id).map(|job| job.view());
        }
        if pending.iter().any(|entry| entry.claimed) || with.jobs.live(id) {
            import(self, with, &all)?;
            return Err(abandon_refused());
        }
        let reports: Vec<_> = pending.iter().map(abandoned).collect();
        if !journal.abandon(&pending, &reports).map_err(failure)? {
            import(self, with, &all_records(&**journal)?)?;
            return Err(abandon_refused());
        }
        // Publish only after the durable transaction. A lost reply or a crash
        // here leaves terminal receipts which the next process imports.
        import(self, with, &all_records(&**journal)?)?;
        let result = self.announcement_job(client, id)?.view();
        self.record(
            client,
            format!("abandon_announcement_recovery: {id}"),
            "allow".into(),
            result.done.clone(),
            None,
        );
        Ok(result)
    }

    /// A new clip cannot overtake a retained recovery in the same household.
    /// Other households remain available, and active jobs in this process are
    /// serialized by the announcer's ordinary household turns.
    pub(super) fn announcement_recovery_ready(
        &self,
        households: &[&HouseholdState],
    ) -> Result<(), Failure> {
        let Some(with) = &self.announcements else {
            return Ok(());
        };
        let Some(journal) = &with.journal else {
            return Ok(());
        };
        let pending = records(&**journal)?;
        if pending.iter().any(|entry| {
            !with.jobs.live(&entry.context.id)
                && households
                    .iter()
                    .any(|household| same_household(&entry.recovery, household))
        }) {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "an interrupted announcement in this household is still being inspected",
            )
            .with_hint("Read the announcement jobs for the restoration outcome. To keep current playback and stop pending recovery, explicitly abandon it; claimed cleanup must finish first. Playback controls remain available."));
        }
        Ok(())
    }
}

struct Running<'a>(&'a AtomicBool);

impl Drop for Running<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// A shutdown fences every restart write. Ordinary live announcements keep
/// their own cancellation/cleanup drain, which is intentionally independent.
struct RestartCheckpoint<'a> {
    inner: Arc<dyn AnnouncementCheckpoint>,
    with: &'a Announcements,
    context: &'a AnnouncementContext,
}

impl AnnouncementCheckpoint for RestartCheckpoint<'_> {
    fn save(&self, recovery: &AnnouncementRecovery) -> Result<(), CoreError> {
        self.inner.save(recovery)
    }

    fn claim(&self, recovery: &AnnouncementRecovery) -> Result<bool, CoreError> {
        if *self
            .with
            .stopping
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Ok(false);
        }
        self.inner.claim(recovery)
    }

    fn complete(
        &self,
        recovery: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError> {
        let mut report = report.clone();
        sleep_notes(self.context, &mut report);
        self.inner.complete(recovery, &report)
    }

    fn current(&self, recovery: &AnnouncementRecovery) -> bool {
        !*self
            .with
            .stopping
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            && self.inner.current(recovery)
    }
}

fn sleep_notes(context: &AnnouncementContext, report: &mut HouseholdAnnouncement) {
    for coordinator in &context.interrupted_sleeps {
        report.restore_failed.push((coordinator.clone(),
            "the interrupted sleep fade was not resumed after restart; no new sleep timer was created".into()));
    }
}

fn abandon_refused() -> Failure {
    Failure::new(
        ErrorCode::NotReady,
        "announcement recovery changed or cleanup was already claimed; no recovery was abandoned by this request",
    )
    .with_hint("Read this announcement again. Claimed cleanup cannot be abandoned or cancelled; wait for its restoration receipt before another action.")
}

fn abandoned(entry: &PendingAnnouncement) -> HouseholdAnnouncement {
    let mut report = waiting(
        entry,
        "recovery abandoned by explicit request; no speaker commands were sent",
    );
    report.restore_failed.push((
        entry.recovery.lead().clone(),
        "previous playback, grouping, position and levels were not restored; current playback was left as found and future recovery was stopped".into(),
    ));
    sleep_notes(&entry.context, &mut report);
    report
}

fn waiting(entry: &PendingAnnouncement, why: &str) -> HouseholdAnnouncement {
    HouseholdAnnouncement {
        lead: entry.recovery.lead().clone(),
        rooms: entry.recovery.rooms(),
        levels: Vec::new(),
        outcome: Err(why.to_owned()),
        restored: Vec::new(),
        restore_failed: Vec::new(),
    }
}

fn all_records(journal: &dyn AnnouncementJournal) -> Result<Vec<PendingAnnouncement>, Failure> {
    let mut all = journal.records().map_err(failure)?;
    if all.len() > 64 || all.iter().any(|entry| !entry.valid()) {
        return Err(failure("records are invalid or exceed the bound"));
    }
    let mut ids = HashSet::new();
    let mut jobs: HashMap<&str, &AnnouncementContext> = HashMap::new();
    for entry in &all {
        if !ids.insert(entry.recovery.id()) {
            return Err(failure("duplicate household recovery identity"));
        }
        if let Some(context) = jobs.insert(&entry.context.id, &entry.context)
            && (context.client != entry.context.client
                || context.kind != entry.context.kind
                || context.rooms != entry.context.rooms
                || context.created_at != entry.context.created_at)
        {
            return Err(failure(
                "household records disagree about their original announcement",
            ));
        }
    }
    all.sort_by(|a, b| {
        a.context
            .created_at
            .cmp(&b.context.created_at)
            .then_with(|| a.context.id.cmp(&b.context.id))
            .then_with(|| a.recovery.id().cmp(b.recovery.id()))
    });
    Ok(all)
}

fn import(
    surface: &Surface,
    with: &Announcements,
    all: &[PendingAnnouncement],
) -> Result<(), Failure> {
    let mut jobs: Vec<(&AnnouncementContext, Vec<&PendingAnnouncement>)> = Vec::new();
    for entry in all {
        if let Some((_, entries)) = jobs
            .iter_mut()
            .find(|(context, _)| context.id == entry.context.id)
        {
            entries.push(entry);
        } else {
            jobs.push((&entry.context, vec![entry]));
        }
    }
    for (context, entries) in jobs {
        let pending = entries.iter().any(|entry| entry.report.is_none());
        let households: Vec<_> = entries
            .iter()
            .map(|entry| entry.recovery.household_state().clone())
            .collect();
        let observations = with
            .recovery_observations
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let report = AnnounceReport { households: entries.iter().map(|entry| {
            entry.report.clone().or_else(|| observations.get(entry.recovery.id()).cloned())
                .unwrap_or_else(|| waiting(entry,
                    "announcement interrupted; waiting for fresh ownership evidence before restoration"))
        }).collect() };
        drop(observations);
        let progress = entries
            .iter()
            .map(|entry| AnnouncementProgressDto {
                household: Some(entry.recovery.household().0.clone()),
                lead: entry.recovery.lead().0.clone(),
                rooms: entry.recovery.rooms(),
                phase: if entry.report.is_some() {
                    "complete"
                } else {
                    "restoring"
                }
                .into(),
            })
            .collect();
        let mut dto = AnnounceDto::from_report(&households, &report);
        // A household may have completed before another ever checkpointed.
        // Never turn its surviving partial record into a whole-job success.
        let recorded_rooms: usize = entries
            .iter()
            .map(|entry| entry.recovery.rooms().len())
            .sum();
        if recorded_rooms < context.rooms.len() {
            dto.clean = false;
            dto.done.push_str("; some requested rooms have no retained delivery receipt; their outcome is unknown");
        }
        if pending {
            dto.done = format!("interrupted announcement recovery pending; {}", dto.done);
        }
        with.jobs.import(
            context,
            dto,
            progress,
            pending,
            pending
                && entries
                    .iter()
                    .all(|entry| entry.report.is_some() || !entry.claimed),
            surface.clock.now().timestamp(),
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests;
