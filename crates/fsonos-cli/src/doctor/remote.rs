//! Read the daemon's doctor report without touching the caller's LAN or store.
//!
//! Check ids in the core report are static registry keys. The wire reader owns
//! them, while contract tests keep its text, JSON and exit codes aligned with
//! the core renderer.

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::doctor::{EXIT_FAIL, EXIT_OK, EXIT_WARN, JSON_SCHEMA, Status};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map, Value};
use std::fmt::Write as _;

/// An explicit remote diagnosis stays remote, including when the daemon
/// refuses the request or returns an unreadable report. The ordinary command
/// still diagnoses this host, and --direct overrides an environment URL.
pub(super) fn fetch(global: &GlobalArgs, prefix: Option<&str>) -> Result<Option<Report>, Failure> {
    if global.direct || (!global.daemon && global.daemon_url.is_none()) {
        return Ok(None);
    }
    let daemon = Daemon::find(global, true)?
        .ok_or_else(|| Failure::new(ErrorCode::NotReady, "doctor requires a running daemon"))?;
    let report: Report = daemon.get("/doctor")?;
    report.checked(prefix).map(Some)
}

#[derive(Debug, Deserialize, Serialize)]
pub(super) struct Report {
    schema: u32,
    exit_code: u8,
    counts: Counts,
    checks: Vec<Check>,
    // A newer daemon may add compatible evidence without changing schema.
    #[serde(flatten)]
    extra: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
struct Counts {
    pass: usize,
    warn: usize,
    fail: usize,
    skip: usize,
}

#[derive(Debug, Deserialize, Serialize)]
struct Check {
    id: String,
    title: String,
    #[serde(deserialize_with = "status")]
    status: Status,
    summary: String,
    detail: Option<String>,
    remedy: Option<String>,
    evidence: Value,
    duration_ms: u64,
    #[serde(flatten)]
    extra: Map<String, Value>,
}

fn status<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Status, D::Error> {
    let value = String::deserialize(deserializer)?;
    match value.as_str() {
        "pass" => Ok(Status::Pass),
        "warn" => Ok(Status::Warn),
        "fail" => Ok(Status::Fail),
        "skip" => Ok(Status::Skip),
        _ => Err(serde::de::Error::unknown_variant(
            &value,
            &["pass", "warn", "fail", "skip"],
        )),
    }
}

impl Counts {
    fn exit_code(self) -> u8 {
        if self.fail > 0 {
            EXIT_FAIL
        } else if self.warn > 0 {
            EXIT_WARN
        } else {
            EXIT_OK
        }
    }
}

impl Report {
    fn tally(&self) -> Counts {
        let mut counts = Counts::default();
        for check in &self.checks {
            match check.status {
                Status::Pass => counts.pass += 1,
                Status::Warn => counts.warn += 1,
                Status::Fail => counts.fail += 1,
                Status::Skip => counts.skip += 1,
            }
        }
        counts
    }

    fn checked(mut self, prefix: Option<&str>) -> Result<Self, Failure> {
        if self.schema != JSON_SCHEMA {
            return Err(Failure::new(
                ErrorCode::Internal,
                format!(
                    "the daemon's doctor report uses schema {}, this CLI understands {}",
                    self.schema, JSON_SCHEMA
                ),
            )
            .with_hint("Update the CLI and daemon to compatible versions."));
        }
        let counts = self.tally();
        if self.counts != counts || self.exit_code != counts.exit_code() {
            return Err(Failure::new(
                ErrorCode::Internal,
                "the daemon's doctor report has inconsistent counts or exit code",
            ));
        }
        if let Some(prefix) = prefix {
            self.checks.retain(|check| check.id.starts_with(prefix));
            self.counts = self.tally();
            self.exit_code = self.counts.exit_code();
        }
        Ok(self)
    }

    pub(super) fn exit_code(&self) -> u8 {
        self.exit_code
    }

    pub(super) fn render_table(&self) -> String {
        let mut out = String::new();
        for check in &self.checks {
            let _ = write!(
                out,
                "{} {} — {}",
                check.status.glyph(),
                check.title,
                check.summary
            );
            if check.status != Status::Skip {
                let _ = write!(out, " ({} ms)", check.duration_ms);
            }
            out.push('\n');
            if let Some(detail) = &check.detail {
                let _ = writeln!(out, "    {detail}");
            }
            if let Some(remedy) = &check.remedy {
                let _ = writeln!(out, "    → {remedy}");
            }
        }
        let counts = self.counts;
        let _ = writeln!(
            out,
            "\n{} checks: {} passed, {} warned, {} failed, {} skipped",
            self.checks.len(),
            counts.pass,
            counts.warn,
            counts.fail,
            counts.skip
        );
        out
    }
}

#[cfg(test)]
mod tests;
