//! `selftest.last`: the last full `fsonos selftest` run on this host, as
//! that run kept it in the data directory: the same scrubbed report it
//! printed, when it ran and which fsonos ran it. Doctor reads the file; it
//! runs nothing on the players.

use fsonos_core::doctor::{Check, CheckContext, CheckId, CheckResult, Report};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const LAST: CheckId = CheckId("selftest.last");

/// The kept run, in the data directory.
pub const FILE: &str = "selftest-last.json";

/// Seconds since the Unix epoch, now.
#[must_use]
pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Keep `report`, a full run that ended at `at` (Unix seconds), for doctor.
/// Written beside the file and renamed over it, so a reader never sees half
/// of one.
pub fn save(data_dir: &Path, report: &Report, at: u64) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let kept = json!({
        "at": at,
        "version": env!("CARGO_PKG_VERSION"),
        "report": report.to_json(),
    });
    let partial = data_dir.join(format!("{FILE}.partial"));
    std::fs::write(&partial, kept.to_string())?;
    std::fs::rename(&partial, data_dir.join(FILE))
}

/// See the module docs.
pub struct LastSelftest {
    pub data_dir: PathBuf,
}

impl Check for LastSelftest {
    fn id(&self) -> CheckId {
        LAST
    }

    fn title(&self) -> &'static str {
        "Last self-test"
    }

    fn run(&self, _: &CheckContext) -> CheckResult {
        verdict(&self.data_dir.join(FILE), unix_now())
    }
}

/// The verdict on the run kept at `path`, `now` being Unix seconds.
fn verdict(path: &Path, now: u64) -> CheckResult {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return CheckResult::skip("no self-test recorded yet (run `fsonos selftest`)");
        }
        Err(e) => return unreadable(path, &e.to_string()),
    };
    let Ok(kept) = serde_json::from_str::<Value>(&text) else {
        return unreadable(path, "not JSON");
    };
    let report = &kept["report"];
    let (Some(at), Some(code)) = (kept["at"].as_u64(), report["exit_code"].as_u64()) else {
        return unreadable(path, "no time or exit code");
    };
    let version = kept["version"].as_str().unwrap_or("unknown");
    let ago = age(now.saturating_sub(at));
    let count = |status: &str| report["counts"][status].as_u64().unwrap_or(0);
    let evidence = json!({
        "at": at,
        "version": version,
        "exit_code": code,
        "counts": report["counts"],
    });
    if code == 0 {
        let skipped = match count("skip") {
            0 => String::new(),
            n => format!(", {n} skipped"),
        };
        return CheckResult::pass(format!(
            "{} check(s) passed{skipped}, {ago} ago (fsonos {version})",
            count("pass")
        ))
        .with_evidence(evidence);
    }
    let unpassed: Vec<&str> = report["checks"]
        .as_array()
        .map_or(&[][..], Vec::as_slice)
        .iter()
        .filter(|c| c["status"] == "warn" || c["status"] == "fail")
        .filter_map(|c| c["id"].as_str())
        .collect();
    CheckResult::warn(
        format!(
            "the last self-test, {ago} ago (fsonos {version}), did not pass: {}",
            unpassed.join(", ")
        ),
        "Run `fsonos selftest` for each check's report, and again once it is addressed.",
    )
    .with_evidence(evidence)
}

fn unreadable(path: &Path, why: &str) -> CheckResult {
    CheckResult::warn(
        "the last self-test's record could not be read",
        "Run `fsonos selftest` again to record a new one.",
    )
    .with_detail(format!("{}: {why}", path.display()))
}

/// `secs` as a rough age: "40 s", "12 min", "5 h", "3 d".
fn age(secs: u64) -> String {
    match secs {
        0..120 => format!("{secs} s"),
        120..7_200 => format!("{} min", secs / 60),
        7_200..172_800 => format!("{} h", secs / 3_600),
        _ => format!("{} d", secs / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::doctor::{Runner, Status};

    /// A check that answers `result`.
    struct Fixed(&'static str, CheckResult);

    impl Check for Fixed {
        fn id(&self) -> CheckId {
            CheckId(self.0)
        }

        fn title(&self) -> &'static str {
            self.0
        }

        fn run(&self, _: &CheckContext) -> CheckResult {
            self.1.clone()
        }
    }

    /// A new, empty directory of this test's own.
    fn scratch(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "fsonos-selftest-last-{}-{nonce}-{label}",
            std::process::id()
        ));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn report(results: Vec<(&'static str, CheckResult)>) -> Report {
        let mut runner = Runner::new();
        for (id, result) in results {
            runner.register(Fixed(id, result));
        }
        runner.run().unwrap()
    }

    #[test]
    fn a_kept_run_is_read_back_with_its_age_and_verdict() {
        let dir = scratch("verdicts");
        let at = 1_700_000_000;
        let passed = report(vec![
            ("selftest.house", CheckResult::pass("two households")),
            ("selftest.reads", CheckResult::pass("all read")),
            ("selftest.events", CheckResult::skip("no callback")),
        ]);
        save(&dir, &passed, at).unwrap();
        let ok = verdict(&dir.join(FILE), at + 3 * 3_600);
        assert_eq!(ok.status, Status::Pass, "{ok:?}");
        assert!(
            ok.summary
                .starts_with("2 check(s) passed, 1 skipped, 3 h ago (fsonos "),
            "{ok:?}"
        );
        assert_eq!(ok.evidence["at"], at);

        let warned = report(vec![
            ("selftest.house", CheckResult::pass("two households")),
            (
                "selftest.events",
                CheckResult::warn("one renewal lost", "retry"),
            ),
            (
                "selftest.reads",
                CheckResult::fail("a read failed", "check"),
            ),
        ]);
        save(&dir, &warned, at).unwrap();
        let not = verdict(&dir.join(FILE), at + 40);
        assert_eq!(not.status, Status::Warn, "{not:?}");
        assert!(not.summary.contains("40 s ago"), "{not:?}");
        let named = not.summary.split("did not pass: ").nth(1).unwrap_or("");
        let mut named: Vec<&str> = named.split(", ").collect();
        named.sort_unstable();
        assert_eq!(named, ["selftest.events", "selftest.reads"], "{not:?}");
        assert!(!dir.join(format!("{FILE}.partial")).exists());
    }

    #[test]
    fn no_record_is_a_skip_and_a_broken_one_a_warning() {
        let dir = scratch("broken");
        let path = dir.join(FILE);
        assert_eq!(verdict(&path, 0).status, Status::Skip);
        std::fs::write(&path, "{ half").unwrap();
        let broken = verdict(&path, 0);
        assert_eq!(broken.status, Status::Warn);
        assert!(broken.detail.unwrap().contains("not JSON"));
        std::fs::write(&path, r#"{"report": {}}"#).unwrap();
        assert_eq!(verdict(&path, 0).status, Status::Warn);
    }

    #[test]
    fn ages_read_at_a_glance() {
        assert_eq!(age(0), "0 s");
        assert_eq!(age(119), "119 s");
        assert_eq!(age(120), "2 min");
        assert_eq!(age(7_199), "119 min");
        assert_eq!(age(7_200), "2 h");
        assert_eq!(age(172_800), "2 d");
    }
}
