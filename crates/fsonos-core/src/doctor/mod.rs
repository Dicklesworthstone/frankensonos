//! The diagnostic check engine behind `fsonos doctor`.
//!
//! What makes Sonos control fail is usually invisible: multicast blocked by
//! the OS or the Wi-Fi, a firewall in front of the GENA callback, Spotify not
//! linked in one household, an address the bind guard refuses. Each of those
//! becomes a [`Check`] with a stable [`CheckId`] that reports a named result
//! and a concrete remedy. This module is the engine only — checks plug in
//! from their own modules, and the CLI, HTTP API, MCP server and setup flow
//! all share it.
//!
//! * [`Check`] — one diagnostic; [`CheckResult`] — what it found.
//! * [`Runner`] — runs registered checks in prerequisite order, with a
//!   per-check timeout, and skips the dependents of a failed check.
//! * [`Report`] — the results, their counts and the process exit code, with a
//!   human table ([`Report::render_table`]) and versioned JSON
//!   ([`Report::to_json`]).

mod render;
mod runner;

pub use render::JSON_SCHEMA;
pub use runner::{RegistryError, Runner};

use serde::Serialize;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Exit code when every check passed (or was skipped by its own choice).
pub const EXIT_OK: u8 = 0;
/// Exit code when nothing failed but at least one check warned. Codes 1–5
/// belong to the CLI's error scheme and clap exits 2 on usage errors.
pub const EXIT_WARN: u8 = 6;
/// Exit code when any check failed.
pub const EXIT_FAIL: u8 = 7;

/// A check's stable identifier, dotted by area (e.g. `"lan.ssdp"`,
/// `"spotify.render_params.s1"`). Ids are part of the JSON contract: never
/// rename one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(transparent)]
pub struct CheckId(pub &'static str);

impl fmt::Display for CheckId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}

/// The outcome class of a check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pass,
    Warn,
    Fail,
    /// Not run: a prerequisite failed, or the check does not apply here.
    Skip,
}

/// What a check found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CheckResult {
    pub status: Status,
    /// One line: what was observed.
    pub summary: String,
    /// Optional longer explanation.
    pub detail: Option<String>,
    /// What the owner should do about a warning or failure.
    pub remedy: Option<String>,
    /// Machine-readable facts behind the verdict (addresses, counts, codes).
    pub evidence: serde_json::Value,
    /// Wall time the check took; filled in by the [`Runner`].
    pub duration_ms: u64,
}

impl CheckResult {
    fn new(status: Status, summary: impl Into<String>) -> Self {
        Self {
            status,
            summary: summary.into(),
            detail: None,
            remedy: None,
            evidence: serde_json::Value::Null,
            duration_ms: 0,
        }
    }

    #[must_use]
    pub fn pass(summary: impl Into<String>) -> Self {
        Self::new(Status::Pass, summary)
    }

    #[must_use]
    pub fn warn(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self::new(Status::Warn, summary).with_remedy(remedy)
    }

    #[must_use]
    pub fn fail(summary: impl Into<String>, remedy: impl Into<String>) -> Self {
        Self::new(Status::Fail, summary).with_remedy(remedy)
    }

    /// The check does not apply here (e.g. no S1 household on this LAN).
    #[must_use]
    pub fn skip(summary: impl Into<String>) -> Self {
        Self::new(Status::Skip, summary)
    }

    #[must_use]
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    #[must_use]
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }

    #[must_use]
    pub fn with_evidence(mut self, evidence: serde_json::Value) -> Self {
        self.evidence = evidence;
        self
    }
}

/// One diagnostic.
///
/// **Read-only contract:** a check observes; it never changes playback,
/// volume, grouping, queues or favorites. A check that creates anything
/// transient (a GENA subscription, a listener) must remove it before
/// returning — including when [`CheckContext::is_cancelled`] turns true
/// because its timeout passed.
///
/// Checks run on their own thread, so they capture what they need (a
/// transport, the config, the store) at construction and must be
/// `Send + Sync`.
pub trait Check: Send + Sync {
    /// Stable id; see [`CheckId`].
    fn id(&self) -> CheckId;
    /// Human title for the table (e.g. "SSDP discovery").
    fn title(&self) -> &str;
    /// Checks that must pass (or warn) before this one is worth running.
    fn requires(&self) -> &[CheckId] {
        &[]
    }
    /// Override the runner's per-check timeout.
    fn timeout(&self) -> Option<Duration> {
        None
    }
    /// Run the check. Long-running checks should poll
    /// [`CheckContext::is_cancelled`] and stop (cleaning up) when it is set.
    fn run(&self, ctx: &CheckContext) -> CheckResult;
}

/// Handed to [`Check::run`]: the deadline, and the cancellation flag the
/// runner sets when the deadline passes.
#[derive(Debug, Clone)]
pub struct CheckContext {
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
}

impl CheckContext {
    fn new(timeout: Duration) -> Self {
        Self {
            deadline: Instant::now() + timeout,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// When the runner stops waiting for this check.
    #[must_use]
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    /// Time left before the deadline (zero once it has passed).
    #[must_use]
    pub fn remaining(&self) -> Duration {
        self.deadline.saturating_duration_since(Instant::now())
    }

    /// The runner gave up on this check: stop, clean up, return.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// One check's row in a [`Report`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    pub id: CheckId,
    pub title: String,
    #[serde(flatten)]
    pub result: CheckResult,
}

/// Tallies by [`Status`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Counts {
    pub pass: usize,
    pub warn: usize,
    pub fail: usize,
    pub skip: usize,
}

/// The results of a doctor run, in execution order.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Report {
    pub entries: Vec<Entry>,
}

impl Report {
    #[must_use]
    pub fn counts(&self) -> Counts {
        let mut c = Counts::default();
        for e in &self.entries {
            match e.result.status {
                Status::Pass => c.pass += 1,
                Status::Warn => c.warn += 1,
                Status::Fail => c.fail += 1,
                Status::Skip => c.skip += 1,
            }
        }
        c
    }

    /// [`EXIT_FAIL`] if anything failed, else [`EXIT_WARN`] if anything
    /// warned, else [`EXIT_OK`].
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        let c = self.counts();
        if c.fail > 0 {
            EXIT_FAIL
        } else if c.warn > 0 {
            EXIT_WARN
        } else {
            EXIT_OK
        }
    }

    /// The result for `id`, if it ran (or was skipped).
    #[must_use]
    pub fn get(&self, id: CheckId) -> Option<&CheckResult> {
        self.entries.iter().find(|e| e.id == id).map(|e| &e.result)
    }
}
