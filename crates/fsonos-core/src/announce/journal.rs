//! Durable announcement ownership, separate from process-local delivery jobs.
//! A recovery claim is persisted before cleanup and is never replayable.

mod dispatch;
mod proof;
mod wire;
pub(in crate::announce) use wire::room_vector;

use super::{Announcer, Clip, GroupPlan, HouseholdAnnouncement, recovery};
use crate::snapshot::{Aspect, RestoreReport, ZoneSnapshot};
use crate::{CoreError, HouseholdState, Room};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::{HouseholdId, PlayerId, TransportState};
use serde::{Deserialize, Serialize};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const MAX_AGE_MILLIS: u64 = 24 * 60 * 60 * 1_000;
const MAX_RECORD_BYTES: usize = 4 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(0);

/// The daemon supplies private, atomic persistence. `save` accepts only an
/// unclaimed record. `claim` compares the complete saved record and atomically
/// marks it in flight; a claimed record must never be offered for replay after
/// a restart. `complete` retains a bounded outcome, including partial cleanup.
///
/// `current` is true only for this exact pending record or this caller's claim.
/// Failed reads/locks return false. No method may report success before its
/// durable publication; a storage error before a write prevents that write.
pub trait AnnouncementCheckpoint: Send + Sync {
    fn save(&self, record: &AnnouncementRecovery) -> Result<(), CoreError>;
    fn claim(&self, record: &AnnouncementRecovery) -> Result<bool, CoreError>;
    fn complete(
        &self,
        record: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError>;
    fn current(&self, record: &AnnouncementRecovery) -> bool;
}

/// Private saved-file evidence for one household. This contains native source
/// metadata and physical identities; public job observations should expose only
/// its summary. Preparing records explain incomplete effects but never authorize
/// automatic playback restoration. Established records can return a still-owned
/// clip to its original sources or its unannounced survivors' current playback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnnouncementRecovery {
    version: u8,
    id: String,
    household: HouseholdId,
    created_at: u64,
    #[serde(with = "wire::household")]
    before: HouseholdState,
    plan: GroupPlan,
    selected: Vec<PlayerId>,
    levels: Vec<(String, u8)>,
    snapshots: Vec<ZoneSnapshot>,
    effects: recovery::Effects,
    clip_url: String,
    clip_metadata: String,
    proof: proof::Physical,
    established: bool,
}

impl AnnouncementRecovery {
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    #[must_use]
    pub fn household(&self) -> &HouseholdId {
        &self.household
    }

    #[must_use]
    pub fn household_state(&self) -> &HouseholdState {
        &self.before
    }

    /// Unix milliseconds, fixed before any announcement side effect.
    #[must_use]
    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    #[must_use]
    pub fn recoverable(&self) -> bool {
        self.established && self.effects.preserve.is_none() && self.proof.boot_known()
    }

    #[must_use]
    pub fn lead(&self) -> &PlayerId {
        &self.plan.lead
    }

    /// Exact selected room primaries, independent of duplicate room names.
    #[must_use]
    pub fn selected(&self) -> &[PlayerId] {
        &self.selected
    }

    #[must_use]
    pub fn rooms(&self) -> Vec<String> {
        self.selected
            .iter()
            .map(|id| super::room_name(&self.before, id))
            .collect()
    }

    /// Structural validation only: old terminal receipts remain readable.
    /// The age limit for automatic recovery is checked by `Announcer::recover`.
    #[must_use]
    pub fn valid(&self) -> bool {
        self.version == 1
            && !self.id.is_empty()
            && self.id.len() <= 128
            && !self.household.0.is_empty()
            && self.household.0 == super::turn_key(&self.before)
            && self.created_at > 0
            && !self.selected.is_empty()
            && self.selected.len() <= 64
            && self.before.players.len() <= 256
            && self.before.rooms.len() <= 256
            && self.before.groups.len() <= 256
            && self.plan.zones.len() <= 64
            && self.snapshots.len() <= self.plan.zones.len()
            && self.levels.len() <= self.selected.len()
            && self.levels.iter().all(|(_, level)| *level <= 100)
            && !self.clip_url.is_empty()
            && self.clip_url.len() <= 4096
            && self.clip_metadata.len() <= 16 * 1024
            && self.proof.valid(self)
            && (!self.established
                || (self.snapshots.len() == self.plan.zones.len()
                    && self.effects.installed
                    && self.effects.clip.as_deref() == Some(self.clip_url.as_str())
                    && self.levels.len() == self.selected.len()
                    && self.expected_groups().is_some()))
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_RECORD_BYTES)
    }

    fn report(&self, why: &str) -> HouseholdAnnouncement {
        HouseholdAnnouncement {
            lead: self.plan.lead.clone(),
            rooms: self.rooms(),
            levels: self.levels.clone(),
            outcome: Err(why.into()),
            restored: Vec::new(),
            restore_failed: Vec::new(),
        }
    }

    fn preserved(&self, why: &str) -> HouseholdAnnouncement {
        let mut report = self.report(why);
        report.restored = self
            .plan
            .zones
            .iter()
            .map(|zone| {
                (
                    zone.clone(),
                    RestoreReport {
                        restored: Vec::new(),
                        skipped: Aspect::ALL
                            .into_iter()
                            .map(|aspect| (aspect, why.into()))
                            .collect(),
                    },
                )
            })
            .collect();
        report
    }

    fn recent(&self) -> bool {
        let at = millis();
        at >= self.created_at && at - self.created_at <= MAX_AGE_MILLIS
    }
}

fn millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

pub(super) struct Session<'a> {
    checkpoint: &'a dyn AnnouncementCheckpoint,
    record: AnnouncementRecovery,
    persisted: AnnouncementRecovery,
}

impl<'a> Session<'a> {
    pub(super) fn begin<T: Transport + ?Sized>(
        t: &T,
        household: &HouseholdState,
        plan: &GroupPlan,
        rooms: &[&Room],
        clip: &Clip,
        checkpoint: &'a dyn AnnouncementCheckpoint,
    ) -> Result<Self, CoreError> {
        let created_at = millis();
        let record = AnnouncementRecovery {
            version: 1,
            id: format!(
                "{created_at}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ),
            household: HouseholdId(super::turn_key(household)),
            created_at,
            before: household.clone(),
            plan: plan.clone(),
            selected: rooms.iter().map(|room| room.primary.clone()).collect(),
            levels: Vec::new(),
            snapshots: Vec::new(),
            effects: recovery::Effects::default(),
            clip_url: clip.url.clone(),
            clip_metadata: clip.didl(),
            proof: proof::Physical::capture(t, household, plan)?,
            established: false,
        };
        if !record.valid() {
            return Err(CoreError::Store(
                "announcement recovery evidence is incomplete or too large".into(),
            ));
        }
        checkpoint.save(&record)?;
        Ok(Self {
            checkpoint,
            persisted: record.clone(),
            record,
        })
    }

    fn save(&mut self) -> Result<(), CoreError> {
        if !self.record.valid() {
            return Err(CoreError::Store(
                "announcement recovery evidence is incomplete or too large".into(),
            ));
        }
        self.checkpoint.save(&self.record)?;
        self.persisted = self.record.clone();
        Ok(())
    }

    pub(super) fn finish(
        mut self,
        effects: &recovery::Effects,
        report: &mut HouseholdAnnouncement,
        restore: impl FnOnce(&Claim<'_>, &mut HouseholdAnnouncement),
    ) {
        self.record.effects = effects.clone();
        // An unsuccessful final save does not invent a newer durable record.
        // All actual effects were already recorded before their dispatch.
        if let Err(error) = self.save() {
            report.restore_failed.push((
                self.record.plan.lead.clone(),
                format!("the final announcement checkpoint failed: {error}"),
            ));
        }
        let claim = match Claim::take(self.checkpoint, &self.persisted) {
            Ok(Some(claim)) => claim,
            Ok(None) => {
                report.restore_failed.push((
                    self.record.plan.lead.clone(),
                    "announcement cleanup was already claimed or retired; no cleanup was replayed"
                        .into(),
                ));
                return;
            }
            Err(error) => {
                report.restore_failed.push((self.record.plan.lead.clone(), format!(
                    "could not durably claim announcement cleanup; nothing was restored: {error}"
                )));
                return;
            }
        };
        restore(&claim, report);
        claim.complete(report);
    }
}

pub(super) fn snapshots(
    session: &mut Option<Session<'_>>,
    snapshots: &[ZoneSnapshot],
    levels: &[(String, u8)],
) -> Result<(), CoreError> {
    if let Some(session) = session {
        session.record.snapshots = snapshots.to_vec();
        session.record.levels = levels.to_vec();
        session.save()?;
    }
    Ok(())
}

pub(super) fn effects(
    session: &mut Option<Session<'_>>,
    effects: &recovery::Effects,
) -> Result<(), CoreError> {
    if let Some(session) = session {
        session.record.effects = effects.clone();
        session.save()?;
    }
    Ok(())
}

pub(super) fn established<T: Transport + ?Sized>(
    session: &mut Option<Session<'_>>,
    t: &T,
    effects: &mut recovery::Effects,
) -> Result<(), CoreError> {
    if let Some(session) = session {
        session.record.effects = effects.clone();
        match session.record.observe(t) {
            Ok(Some(_)) => {
                session.record.established = true;
                // If durability fails after Play, cleanup cannot rely on the
                // established marker. Preserve the attempted clip explicitly;
                // its preparation receipt is not automatic restart authority.
                if let Err(error) = session.save() {
                    effects.preserve = Some(format!(
                        "the announcement started but its ownership checkpoint failed; its current state was preserved: {error}"
                    ));
                    return Err(error);
                }
            }
            result => {
                let why = match result {
                    Err(error) => error.to_string(),
                    _ => "the installed announcement no longer has complete ownership evidence"
                        .into(),
                };
                effects.preserve = Some(why.clone());
                return Err(CoreError::Uncertain(why));
            }
        }
    }
    Ok(())
}

/// Every nested control mutation repeats the durable pre-dispatch admission,
/// including compound leave/handoff and SetURI+Play operations.
pub(super) struct Recording<'a, T: Transport + ?Sized> {
    inner: &'a T,
    session: Option<&'a Session<'a>>,
}

pub(super) fn recording<'a, T: Transport + ?Sized>(
    inner: &'a T,
    session: Option<&'a Session<'a>>,
) -> Recording<'a, T> {
    Recording { inner, session }
}

fn mutation(action: &str) -> bool {
    let name = action
        .rsplit('#')
        .next()
        .unwrap_or(action)
        .trim_end_matches('"');
    !name.starts_with("Get") && name != "Browse"
}

impl<T: Transport + ?Sized> Transport for Recording<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if mutation(action)
            && let Some(session) = self.session
        {
            session
                .checkpoint
                .save(&session.persisted)
                .map_err(|error| {
                    ProtoError::Malformed(format!(
                        "announcement checkpoint refused the write: {error}"
                    ))
                })?;
        }
        self.inner.soap_post(host, path, action, body)
    }
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }
}

pub(super) struct Claim<'a> {
    checkpoint: &'a dyn AnnouncementCheckpoint,
    record: &'a AnnouncementRecovery,
}

impl<'a> Claim<'a> {
    fn take(
        checkpoint: &'a dyn AnnouncementCheckpoint,
        record: &'a AnnouncementRecovery,
    ) -> Result<Option<Self>, CoreError> {
        Ok(checkpoint
            .claim(record)?
            .then_some(Self { checkpoint, record }))
    }
    pub(super) fn transport<'t, T: Transport + ?Sized>(&'t self, inner: &'t T) -> Claimed<'t, T> {
        Claimed {
            inner,
            claim: self,
            fence: None,
        }
    }
    fn recovery_transport<'t, T: Transport + ?Sized>(&'t self, inner: &'t T) -> Claimed<'t, T> {
        Claimed {
            inner,
            claim: self,
            fence: Some(dispatch::Fence::new(self.record)),
        }
    }
    fn complete(&self, report: &mut HouseholdAnnouncement) {
        if let Err(error) = self.checkpoint.complete(self.record, report) {
            report.restore_failed.push((self.record.plan.lead.clone(), format!(
                "cleanup finished but its durable result could not be saved; it will not replay: {error}"
            )));
        }
    }
}

pub(super) struct Claimed<'a, T: Transport + ?Sized> {
    inner: &'a T,
    claim: &'a Claim<'a>,
    fence: Option<dispatch::Fence>,
}

impl<T: Transport + ?Sized> Transport for Claimed<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let mut confirmed_stop = false;
        if mutation(action) {
            if !self.claim.checkpoint.current(self.claim.record) {
                return Err(ProtoError::Malformed(
                    "announcement recovery was retired; no further write was sent".into(),
                ));
            }
            self.claim
                .record
                .proof
                .check(self.inner, self.claim.record)
                .map_err(|error| ProtoError::Malformed(error.to_string()))?;
            if !self.claim.record.proof.owns_address(host) {
                return Err(ProtoError::Malformed(
                    "announcement recovery cannot write an unrecorded endpoint".into(),
                ));
            }
            if let Some(fence) = &self.fence {
                confirmed_stop =
                    !fence.before(self.inner, self.claim.record, host, path, action, body)?;
            }
            if !self.claim.checkpoint.current(self.claim.record) {
                return Err(ProtoError::Malformed(
                    "announcement recovery was retired during its reads".into(),
                ));
            }
        }
        if confirmed_stop {
            return Ok(fsonos_proto::soap::envelope(
                &fsonos_proto::soap::AV_TRANSPORT,
                "StopResponse",
                "",
            ));
        }
        self.inner.soap_post(host, path, action, body)
    }
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.inner.http_get(url)
    }
}

impl Announcer {
    /// Return an interrupted, established clip to the original sources without
    /// ever replaying that clip. Preparation-only, changed, expired or already
    /// claimed records produce explicit preserved receipts. Unavailable initial
    /// observations remain pending; after claim every outcome is terminal.
    #[allow(clippy::too_many_lines)] // Claim, mutation and terminal boundaries form one lifecycle.
    pub fn recover<T: Transport + ?Sized>(
        &self,
        t: &T,
        record: &AnnouncementRecovery,
        checkpoint: &dyn AnnouncementCheckpoint,
        restore_cap: Option<&(dyn Fn(&Room) -> u8 + Sync)>,
    ) -> HouseholdAnnouncement {
        let _turn = self.turn(record.household.0.clone());
        let preserved = if !record.valid() {
            Some("the saved announcement evidence is invalid; nothing was restored")
        } else if !record.recent() {
            Some("the saved announcement is outside the recovery window; nothing was restored")
        } else if !record.recoverable() {
            Some(
                "the interrupted announcement never durably established clip ownership; incomplete preparation was left for inspection",
            )
        } else {
            match record.observe(t) {
                Ok(Some(_)) => None,
                Ok(None) => {
                    Some("announcement playback or grouping changed; newer intent was preserved")
                }
                Err(error) => {
                    let mut report = record.report("interrupted announcement recovery is waiting for readable ownership evidence");
                    report
                        .restore_failed
                        .push((record.plan.lead.clone(), error.to_string()));
                    return report;
                }
            }
        };
        let claim =
            match Claim::take(checkpoint, record) {
                Ok(Some(claim)) => claim,
                Ok(None) => return record.preserved(
                    "announcement cleanup was already claimed or retired; no recovery was replayed",
                ),
                Err(error) => {
                    let mut report = record.preserved(
                        "announcement recovery could not be durably claimed; nothing was restored",
                    );
                    report
                        .restore_failed
                        .push((record.plan.lead.clone(), error.to_string()));
                    return report;
                }
            };
        if let Some(why) = preserved {
            let mut report = record.preserved(why);
            claim.complete(&mut report);
            return report;
        }
        let mut report = record.report("announcement delivery was interrupted; its owned state was recovered without replaying the clip");
        let guarded = claim.recovery_transport(t);
        let restored = (|| {
            let state = record.observe(&guarded)?.ok_or_else(|| {
                CoreError::Uncertain(
                    "announcement ownership changed while claiming recovery; nothing was restored"
                        .into(),
                )
            })?;
            if state != TransportState::Stopped {
                let host = record.proof.host(&record.plan.lead)?;
                let sent = soap::stop(&guarded, host);
                let observed = record.observe(&guarded)?;
                if observed != Some(TransportState::Stopped) {
                    return Err(sent.err().map_or_else(
                        || {
                            CoreError::Uncertain(
                                "the announcement stop was not confirmed; it was not replayed"
                                    .into(),
                            )
                        },
                        CoreError::from,
                    ));
                }
            }
            recovery::put_back(
                &guarded,
                &record.before,
                &record.plan,
                &record.snapshots,
                &record.effects,
                &[],
                Instant::now(),
                Instant::now() + self.patience,
                restore_cap,
                &mut report,
            );
            Ok::<(), CoreError>(())
        })();
        if let Err(error) = restored {
            report
                .restore_failed
                .push((record.plan.lead.clone(), error.to_string()));
        }
        claim.complete(&mut report);
        report
    }
}
