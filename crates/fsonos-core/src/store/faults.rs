//! Store failures this process met: the full-disk, corruption and I/O
//! errors no busy retry fixes (vg3s.8).
//!
//! On 2026-10-22 the owner's daemon served on through a full disk while
//! every store write failed: actions went unlogged and could not be undone,
//! and the only trace was a WARN from whichever caller noticed. Every engine
//! error the store returns passes through [`note`]. A storage fault is
//! counted, logged at ERROR at most once a minute (with how many there were
//! since the last line), and [`last`] reports it to `GET /health` and the
//! doctor.

use fsqlite::FrankenError;
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// How often a run of storage faults is logged at ERROR.
const LOG_EVERY: Duration = Duration::from_mins(1);

/// What kind of storage fault an engine error is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The volume (or the database's page limit) is full.
    DiskFull,
    /// The database, its WAL, or the file itself is not what it should be.
    Corrupt,
    /// Reading or writing the files failed otherwise.
    Io,
}

impl Kind {
    /// The wire name: `disk_full`, `corrupt` or `io`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DiskFull => "disk_full",
            Self::Corrupt => "corrupt",
            Self::Io => "io",
        }
    }

    /// The kind of `error` when it is a storage fault, not a busy conflict,
    /// a constraint or a query this build got wrong.
    #[must_use]
    pub fn of(error: &FrankenError) -> Option<Self> {
        match error {
            FrankenError::DatabaseFull => Some(Self::DiskFull),
            FrankenError::Io(e) if is_disk_full(e) => Some(Self::DiskFull),
            FrankenError::DatabaseCorrupt { .. }
            | FrankenError::WalCorrupt { .. }
            | FrankenError::NotADatabase { .. } => Some(Self::Corrupt),
            FrankenError::Io(_)
            | FrankenError::IoRead { .. }
            | FrankenError::IoWrite { .. }
            | FrankenError::ShortRead { .. }
            | FrankenError::CheckpointFailed { .. } => Some(Self::Io),
            _ => None,
        }
    }
}

/// ENOSPC, by kind or, from an engine that wrapped it in its own error, by
/// the text the OS gives it.
fn is_disk_full(error: &std::io::Error) -> bool {
    error.kind() == std::io::ErrorKind::StorageFull
        || error.to_string().contains("No space left on device")
}

/// The newest storage fault, as [`last`] reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fault {
    pub kind: Kind,
    /// Storage faults since this process started.
    pub count: u64,
    /// How long ago the newest was.
    pub ago: Duration,
}

struct State {
    count: u64,
    last: Option<(Instant, Kind)>,
    logged: Option<Instant>,
    unlogged: u64,
}

impl State {
    const fn new() -> Self {
        Self {
            count: 0,
            last: None,
            logged: None,
            unlogged: 0,
        }
    }

    /// Count a fault at `now`. `Some(n)` when an ERROR line is due, `n` the
    /// faults it covers.
    fn record(&mut self, kind: Kind, now: Instant) -> Option<u64> {
        self.count += 1;
        self.unlogged += 1;
        self.last = Some((now, kind));
        let due = self
            .logged
            .is_none_or(|at| now.saturating_duration_since(at) >= LOG_EVERY);
        due.then(|| {
            self.logged = Some(now);
            std::mem::take(&mut self.unlogged)
        })
    }

    fn fault(&self, now: Instant) -> Option<Fault> {
        self.last.map(|(at, kind)| Fault {
            kind,
            count: self.count,
            ago: now.saturating_duration_since(at),
        })
    }
}

static STATE: Mutex<State> = Mutex::new(State::new());

/// Note `error`, which a store operation returned, when it is a storage
/// fault.
pub(super) fn note(error: &FrankenError) {
    let Some(kind) = Kind::of(error) else {
        return;
    };
    let due = STATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .record(kind, Instant::now());
    if let Some(faults) = due {
        tracing::error!(
            kind = kind.as_str(),
            faults,
            "the store failed: {error}; actions are not logged or undoable until it recovers"
        );
    }
}

/// The newest storage fault this process met, if any.
#[must_use]
pub fn last() -> Option<Fault> {
    STATE
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .fault(Instant::now())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn storage_faults_are_told_from_conflicts_and_query_errors() {
        let enospc = std::io::Error::from_raw_os_error(28);
        let wrapped = std::io::Error::other("write: No space left on device (os error 28)");
        for (error, kind) in [
            (FrankenError::DatabaseFull, Some(Kind::DiskFull)),
            (FrankenError::Io(enospc), Some(Kind::DiskFull)),
            (FrankenError::Io(wrapped), Some(Kind::DiskFull)),
            (
                FrankenError::DatabaseCorrupt {
                    detail: "bad page".into(),
                },
                Some(Kind::Corrupt),
            ),
            (FrankenError::IoWrite { page: 7 }, Some(Kind::Io)),
            (
                FrankenError::Io(std::io::Error::other("short write")),
                Some(Kind::Io),
            ),
            (FrankenError::QueryReturnedNoRows, None),
        ] {
            assert_eq!(Kind::of(&error), kind, "{error}");
        }
    }

    #[test]
    fn a_run_of_faults_is_logged_once_a_minute_with_its_count() {
        let mut state = State::new();
        let t0 = Instant::now();
        assert_eq!(state.fault(t0), None);
        assert_eq!(state.record(Kind::DiskFull, t0), Some(1), "the first logs");
        let soon = t0 + Duration::from_secs(10);
        assert_eq!(state.record(Kind::DiskFull, soon), None);
        assert_eq!(state.record(Kind::Io, soon), None);
        let later = t0 + LOG_EVERY;
        assert_eq!(
            state.record(Kind::DiskFull, later),
            Some(3),
            "with the two unlogged"
        );
        let now = later + Duration::from_secs(5);
        assert_eq!(
            state.fault(now),
            Some(Fault {
                kind: Kind::DiskFull,
                count: 4,
                ago: Duration::from_secs(5),
            })
        );
    }
}
