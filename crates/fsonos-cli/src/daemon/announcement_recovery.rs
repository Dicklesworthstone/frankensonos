//! Private announcement checkpoints survive the daemon, but never grant a
//! second attempt at an uncertain restoration. The data-directory lock owns
//! the daemon; this stable file lock serializes each journal transaction.

use fsonos_api::surface::announce::{
    AnnouncementContext, AnnouncementJournal, PendingAnnouncement,
};
use fsonos_core::CoreError;
use fsonos_core::announce::{AnnouncementCheckpoint, AnnouncementRecovery, HouseholdAnnouncement};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const FILE: &str = "announcement-recovery.json";
const LOCK: &str = "announcement-recovery.lock";
const VERSION: u32 = 1;
const MAX_BYTES: usize = 8 * 1024 * 1024;
const MAX_RECORDS: usize = 64;

/// Keep unavailable households pending until discovery catches up. Recovery
/// never replays a clip, and a durable claim prevents a failed cleanup from
/// being retried by this worker or a subsequent daemon.
pub(super) fn start(
    surface: &Arc<fsonos_api::Surface>,
    stop: &Arc<AtomicBool>,
) -> io::Result<JoinHandle<()>> {
    let surface = Arc::clone(surface);
    let stop = Arc::clone(stop);
    thread::Builder::new()
        .name("fsonos-announcement-recovery".into())
        .spawn(move || {
            while !stop.load(Ordering::Acquire) {
                match surface.recover_announcements() {
                    Ok(pass) if pass.finished > 0 => tracing::info!(
                        recovered = pass.finished,
                        pending = pass.pending,
                        "finished interrupted announcement recovery"
                    ),
                    Ok(_) => {}
                    Err(error) => tracing::warn!(
                        detail = %error.detail,
                        "announcement recovery awaits a readable journal"
                    ),
                }
                let next = Instant::now() + Duration::from_secs(5);
                while !stop.load(Ordering::Acquire) && Instant::now() < next {
                    thread::sleep(Duration::from_millis(100));
                }
            }
        })
}

#[derive(Clone)]
pub(super) struct Journal {
    state: Arc<Shared>,
}

struct Shared {
    path: PathBuf,
    lock: PathBuf,
    owner: String,
    next_owner: AtomicU64,
    live_owners: Mutex<HashSet<String>>,
    writing: Mutex<()>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ledger {
    version: u32,
    records: Vec<Record>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: VERSION,
            records: Vec::new(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    context: AnnouncementContext,
    recovery: AnnouncementRecovery,
    state: State,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
enum State {
    Pending,
    Claimed { owner: String },
    Complete { report: HouseholdAnnouncement },
}

impl Record {
    fn matches(&self, context: &AnnouncementContext, recovery: &AnnouncementRecovery) -> bool {
        self.context == *context && self.recovery == *recovery
    }

    fn view(&self) -> PendingAnnouncement {
        PendingAnnouncement {
            context: self.context.clone(),
            recovery: self.recovery.clone(),
            claimed: matches!(self.state, State::Claimed { .. }),
            report: match &self.state {
                State::Complete { report } => Some(report.clone()),
                State::Pending | State::Claimed { .. } => None,
            },
        }
    }
}

fn failure(error: impl std::fmt::Display) -> CoreError {
    CoreError::Store(format!("announcement recovery journal: {error}"))
}

impl Journal {
    pub(super) fn new(directory: &Path) -> Result<Self, CoreError> {
        Ok(Self {
            state: Arc::new(Shared {
                path: directory.join(FILE),
                lock: directory.join(LOCK),
                owner: fsonos_core::announce::clip::clip_id().map_err(failure)?,
                next_owner: AtomicU64::new(0),
                live_owners: Mutex::new(HashSet::new()),
                writing: Mutex::new(()),
            }),
        })
    }

    /// Read the previous revision under the same lock used to publish. A
    /// missing file is an empty journal; a corrupt one is never overwritten.
    fn transaction<T>(
        &self,
        change: impl FnOnce(&mut Ledger) -> Result<(T, bool), CoreError>,
    ) -> Result<T, CoreError> {
        self.transaction_retaining(None, change)
    }

    fn transaction_retaining<T>(
        &self,
        retained_job: Option<&str>,
        change: impl FnOnce(&mut Ledger) -> Result<(T, bool), CoreError>,
    ) -> Result<T, CoreError> {
        let _writing = self
            .state
            .writing
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = options.open(&self.state.lock).map_err(failure)?;
        lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => failure("another writer owns the journal"),
            std::fs::TryLockError::Error(error) => failure(error),
        })?;
        let mut ledger = self.read()?;
        let (result, changed) = change(&mut ledger)?;
        if changed {
            // Never evict a pending or claimed recovery to make room. Only
            // old terminal receipts may yield to a new protected operation.
            let text = loop {
                let text = serde_json::to_string(&ledger).map_err(failure)?;
                if ledger.records.len() <= MAX_RECORDS && text.len() <= MAX_BYTES {
                    break text;
                }
                let oldest = ledger
                    .records
                    .iter()
                    .enumerate()
                    .filter(|(_, record)| {
                        matches!(record.state, State::Complete { .. })
                            && retained_job != Some(record.context.id.as_str())
                    })
                    .min_by_key(|(_, record)| record.recovery.created_at())
                    .map(|(index, _)| index)
                    .ok_or_else(|| failure("pending recovery records exceed the journal limit"))?;
                ledger.records.remove(oldest);
            };
            crate::dj::saved::write(&self.state.path, &text).map_err(failure)?;
        }
        Ok(result)
    }

    fn read(&self) -> Result<Ledger, CoreError> {
        let mut bytes = Vec::new();
        match File::open(&self.state.path) {
            Ok(file) => {
                file.take((MAX_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map_err(failure)?;
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(Ledger::default());
            }
            Err(error) => return Err(failure(error)),
        }
        if bytes.len() > MAX_BYTES {
            return Err(failure("saved journal exceeds 8 MiB"));
        }
        let ledger: Ledger = serde_json::from_slice(&bytes).map_err(failure)?;
        if ledger.version != VERSION || ledger.records.len() > MAX_RECORDS {
            return Err(failure("unsupported journal version or record count"));
        }
        let mut ids = HashSet::new();
        for record in &ledger.records {
            if !record.recovery.valid()
                || !ids.insert(record.recovery.id())
                || !record.context.valid()
            {
                return Err(failure("saved journal has invalid or ambiguous identities"));
            }
        }
        Ok(ledger)
    }
}

struct Checkpoint {
    journal: Journal,
    context: AnnouncementContext,
    owner: String,
}

impl Drop for Checkpoint {
    fn drop(&mut self) {
        self.journal
            .state
            .live_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.owner);
    }
}

impl AnnouncementJournal for Journal {
    fn checkpoint(&self, context: &AnnouncementContext) -> Arc<dyn AnnouncementCheckpoint> {
        let owner = format!(
            "{}-{}",
            self.state.owner,
            self.state.next_owner.fetch_add(1, Ordering::Relaxed)
        );
        self.state
            .live_owners
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(owner.clone());
        Arc::new(Checkpoint {
            journal: self.clone(),
            context: context.clone(),
            owner,
        })
    }

    fn records(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        self.transaction(|ledger| Ok((ledger.records.iter().map(Record::view).collect(), false)))
    }

    fn pending(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        self.transaction(|ledger| {
            Ok((
                ledger
                    .records
                    .iter()
                    .filter(|record| !matches!(record.state, State::Complete { .. }))
                    .map(Record::view)
                    .collect(),
                false,
            ))
        })
    }

    fn completed(&self) -> Result<Vec<PendingAnnouncement>, CoreError> {
        self.transaction(|ledger| {
            Ok((
                ledger
                    .records
                    .iter()
                    .filter(|record| matches!(record.state, State::Complete { .. }))
                    .map(Record::view)
                    .collect(),
                false,
            ))
        })
    }

    fn abandon(
        &self,
        pending: &[PendingAnnouncement],
        reports: &[HouseholdAnnouncement],
    ) -> Result<bool, CoreError> {
        let Some(first) = pending.first() else {
            return Ok(false);
        };
        let mut ids = HashSet::new();
        if pending.len() != reports.len()
            || pending.iter().any(|entry| {
                !entry.valid()
                    || entry.claimed
                    || entry.report.is_some()
                    || entry.context.id != first.context.id
                    || entry.context.client != first.context.client
                    || !ids.insert(entry.recovery.id())
            })
        {
            return Err(failure("invalid recovery abandonment request"));
        }
        // Retain this job's prior and newly abandoned household receipts in
        // the same publication. If they cannot fit, leave the pending proof
        // unchanged instead of reporting a success that was already evicted.
        self.transaction_retaining(Some(&first.context.id), |ledger| {
            let unfinished: Vec<_> = ledger
                .records
                .iter()
                .filter(|record| {
                    record.context.id == first.context.id
                        && !matches!(record.state, State::Complete { .. })
                })
                .collect();
            if unfinished.len() != pending.len()
                || unfinished.iter().any(|record| {
                    !matches!(record.state, State::Pending)
                        || !pending
                            .iter()
                            .any(|entry| record.matches(&entry.context, &entry.recovery))
                })
            {
                return Ok((false, false));
            }
            // This lock is also the claim lock. Validate the complete pending
            // set first; a claimed household never permits partial retirement.
            for (entry, report) in pending.iter().zip(reports) {
                let record = ledger
                    .records
                    .iter_mut()
                    .find(|record| record.matches(&entry.context, &entry.recovery))
                    .expect("the exact unclaimed set was checked under this transaction");
                record.state = State::Complete {
                    report: report.clone(),
                };
            }
            Ok((true, true))
        })
    }

    fn retire_claimed(
        &self,
        pending: &PendingAnnouncement,
        report: &HouseholdAnnouncement,
    ) -> Result<bool, CoreError> {
        self.transaction(|ledger| {
            let live = self
                .state
                .live_owners
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            let Some(record) = ledger.records.iter_mut().find(|record| {
                record.matches(&pending.context, &pending.recovery)
                    && matches!(&record.state, State::Claimed { owner } if !live.contains(owner))
            }) else {
                return Ok((false, false));
            };
            record.state = State::Complete {
                report: report.clone(),
            };
            Ok((true, true))
        })
    }
}

impl AnnouncementCheckpoint for Checkpoint {
    fn save(&self, recovery: &AnnouncementRecovery) -> Result<(), CoreError> {
        if !recovery.valid() || !self.context.valid() {
            return Err(failure("refusing an invalid recovery checkpoint"));
        }
        self.journal.transaction(|ledger| {
            if let Some(record) = ledger
                .records
                .iter_mut()
                .find(|record| record.recovery.id() == recovery.id())
            {
                if record.context != self.context
                    || record.recovery.household() != recovery.household()
                    || record.recovery.created_at() != recovery.created_at()
                    || !matches!(record.state, State::Pending)
                {
                    return Err(failure("recovery checkpoint is no longer writable"));
                }
                record.recovery = recovery.clone();
            } else {
                ledger.records.push(Record {
                    context: self.context.clone(),
                    recovery: recovery.clone(),
                    state: State::Pending,
                });
            }
            Ok(((), true))
        })
    }

    fn claim(&self, recovery: &AnnouncementRecovery) -> Result<bool, CoreError> {
        self.journal.transaction(|ledger| {
            let Some(record) = ledger.records.iter_mut().find(|record| {
                record.matches(&self.context, recovery) && matches!(record.state, State::Pending)
            }) else {
                return Ok((false, false));
            };
            record.state = State::Claimed {
                owner: self.owner.clone(),
            };
            Ok((true, true))
        })
    }

    fn complete(
        &self,
        recovery: &AnnouncementRecovery,
        report: &HouseholdAnnouncement,
    ) -> Result<(), CoreError> {
        self.journal.transaction(|ledger| {
            let record = ledger
                .records
                .iter_mut()
                .find(|record| record.matches(&self.context, recovery))
                .ok_or_else(|| failure("recovery checkpoint changed before completion"))?;
            match &record.state {
                State::Claimed { owner } if *owner == self.owner => {}
                State::Complete { report: previous } if previous == report => {
                    return Ok(((), false));
                }
                State::Pending | State::Claimed { .. } | State::Complete { .. } => {
                    return Err(failure("another recovery owns this completion"));
                }
            }
            record.state = State::Complete {
                report: report.clone(),
            };
            Ok(((), true))
        })
    }

    fn current(&self, recovery: &AnnouncementRecovery) -> bool {
        self.journal
            .transaction(|ledger| {
                Ok((
                    ledger.records.iter().any(|record| {
                        record.matches(&self.context, recovery)
                            && match &record.state {
                                State::Pending => true,
                                State::Claimed { owner } => *owner == self.owner,
                                State::Complete { .. } => false,
                            }
                    }),
                    false,
                ))
            })
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests;
