//! `store.disk`: the room left on the volume that holds the store.
//!
//! A full disk does not stop the daemon. It serves on while every store
//! write fails, so actions go unlogged and cannot be undone, and nothing
//! said why (live, 2026-10-22: vg3s.8). This check names the free space
//! before that happens, and the remedy once it has. The thresholds are
//! absolute: the store itself grows by kilobytes a day, what fills a disk is
//! other software, and a percentage would warn about tens of free gigabytes
//! on a large volume.

use super::{Check, CheckContext, CheckId, CheckResult, Runner};
use serde_json::json;
use std::path::{Path, PathBuf};

pub const STORE_DISK: CheckId = CheckId("store.disk");

/// Under this much free space the check warns.
pub const WARN_BELOW: u64 = 5 << 30;
/// Under this much it fails: a few large writes by anything on the volume
/// leave the store none.
pub const FAIL_BELOW: u64 = 1 << 30;

/// Free and total bytes on a volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Space {
    /// What this user may still write (`f_bavail`, not the root reserve).
    pub free: u64,
    pub total: u64,
}

impl Space {
    /// `ok`, `low` (under [`WARN_BELOW`]) or `critical` (under
    /// [`FAIL_BELOW`]), as `GET /health` reports it.
    #[must_use]
    pub fn verdict(&self) -> &'static str {
        if self.free < FAIL_BELOW {
            "critical"
        } else if self.free < WARN_BELOW {
            "low"
        } else {
            "ok"
        }
    }
}

/// The space on the volume that holds `path`, or its nearest existing
/// ancestor (a data directory not made yet lands where it will be made).
///
/// # Errors
/// The volume could not be read, or this is not a unix host.
pub fn space(path: &Path) -> std::io::Result<Space> {
    let existing = path.ancestors().find(|p| p.exists()).unwrap_or(path);
    statvfs(existing)
}

#[cfg(unix)]
fn statvfs(path: &Path) -> std::io::Result<Space> {
    let vfs = rustix::fs::statvfs(path)?;
    Ok(Space {
        free: vfs.f_bavail.saturating_mul(vfs.f_frsize),
        total: vfs.f_blocks.saturating_mul(vfs.f_frsize),
    })
}

#[cfg(not(unix))]
fn statvfs(_: &Path) -> std::io::Result<Space> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "free space is read on unix hosts only",
    ))
}

/// Register `store.disk` for `data_dir`.
pub fn register(runner: &mut Runner, data_dir: PathBuf) {
    runner.register(DiskCheck { data_dir });
}

struct DiskCheck {
    data_dir: PathBuf,
}

impl Check for DiskCheck {
    fn id(&self) -> CheckId {
        STORE_DISK
    }
    fn title(&self) -> &'static str {
        "Free disk space"
    }
    fn run(&self, _ctx: &CheckContext) -> CheckResult {
        judge(&self.data_dir, space(&self.data_dir), held(&self.data_dir))
    }
}

/// What `space`, read for `data_dir` whose files hold `held` bytes, means
/// for the store.
#[must_use]
pub fn judge(data_dir: &Path, space: std::io::Result<Space>, held: u64) -> CheckResult {
    let space = match space {
        Ok(space) => space,
        Err(e) => {
            return CheckResult::skip(format!(
                "the free space on {} is unknown: {e}",
                data_dir.display()
            ));
        }
    };
    let summary = format!(
        "{} free of {} on the store's volume",
        gib(space.free),
        gib(space.total)
    );
    let remedy = format!(
        "Make room on the volume that holds {} (the data directory itself holds {} MiB): when \
         it fills, every store write fails, and actions go unlogged and cannot be undone. fsonos \
         deletes nothing itself.",
        data_dir.display(),
        held >> 20
    );
    let result = match space.verdict() {
        "critical" => CheckResult::fail(summary, remedy),
        "low" => CheckResult::warn(summary, remedy),
        _ => CheckResult::pass(summary),
    };
    result.with_evidence(json!({
        "path": data_dir,
        "free_bytes": space.free,
        "total_bytes": space.total,
        "data_dir_bytes": held,
    }))
}

/// The bytes the files directly in `data_dir` hold (the store, its WAL, the
/// logs and state files); 0 when it cannot be read.
fn held(data_dir: &Path) -> u64 {
    std::fs::read_dir(data_dir).map_or(0, |entries| {
        entries
            .filter_map(|e| e.ok()?.metadata().ok())
            .filter(std::fs::Metadata::is_file)
            .map(|m| m.len())
            .sum()
    })
}

/// `bytes` in GiB to one decimal, without a float.
fn gib(bytes: u64) -> String {
    let tenths = u128::from(bytes) * 10 / (1 << 30);
    format!("{}.{} GiB", tenths / 10, tenths % 10)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::Status;

    fn with_free(free: u64) -> CheckResult {
        let space = Space {
            free,
            total: 100 << 30,
        };
        judge(Path::new("/data"), Ok(space), 3 << 20)
    }

    #[test]
    fn warns_under_five_gib_and_fails_under_one() {
        assert_eq!(with_free(WARN_BELOW).status, Status::Pass);
        assert_eq!(with_free(WARN_BELOW - 1).status, Status::Warn);
        assert_eq!(with_free(FAIL_BELOW).status, Status::Warn);
        assert_eq!(with_free(FAIL_BELOW - 1).status, Status::Fail);
        assert_eq!(with_free(0).status, Status::Fail);
    }

    #[test]
    fn names_the_numbers_and_the_remedy() {
        let low = with_free(2 << 30);
        assert_eq!(
            low.summary,
            "2.0 GiB free of 100.0 GiB on the store's volume"
        );
        let remedy = low.remedy.unwrap();
        assert!(remedy.contains("/data"), "{remedy}");
        assert!(remedy.contains("holds 3 MiB"), "{remedy}");
        let evidence = low.evidence;
        assert_eq!(evidence["free_bytes"], 2_u64 << 30);
        assert_eq!(evidence["data_dir_bytes"], 3_u64 << 20);
        assert_eq!(with_free(WARN_BELOW).remedy, None);
    }

    #[test]
    fn an_unreadable_volume_is_skipped_not_passed() {
        let unknown = judge(Path::new("/data"), Err(std::io::Error::other("no")), 0);
        assert_eq!(unknown.status, Status::Skip);
        assert!(
            unknown.summary.contains("unknown: no"),
            "{}",
            unknown.summary
        );
    }

    #[test]
    fn verdicts_for_health() {
        let at = |free| Space { free, total: free }.verdict();
        assert_eq!(at(WARN_BELOW), "ok");
        assert_eq!(at(WARN_BELOW - 1), "low");
        assert_eq!(at(FAIL_BELOW - 1), "critical");
    }

    #[test]
    fn gib_rounds_down_to_a_tenth() {
        assert_eq!(gib(0), "0.0 GiB");
        assert_eq!(gib((1 << 30) + (1 << 29)), "1.5 GiB");
        assert_eq!(gib((1 << 30) - 1), "0.9 GiB");
        assert_eq!(gib(u64::MAX), "17179869183.9 GiB");
    }

    #[cfg(unix)]
    #[test]
    fn reads_the_volume_of_a_directory_not_made_yet() {
        let dir = tempfile::tempdir().unwrap();
        let space = space(&dir.path().join("not/made/yet")).unwrap();
        assert!(space.total > 0 && space.free <= space.total, "{space:?}");
        std::fs::write(dir.path().join("fsonos.db"), [0_u8; 4096]).unwrap();
        assert_eq!(held(dir.path()), 4096);
        assert_eq!(held(&dir.path().join("missing")), 0);
    }
}
