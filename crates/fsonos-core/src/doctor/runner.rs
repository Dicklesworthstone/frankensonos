//! Running registered checks: prerequisite order, timeouts, skip propagation.

use super::{Check, CheckContext, CheckId, CheckResult, Entry, Report, Status};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

/// A set of checks that cannot be run as registered.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegistryError {
    #[error("doctor check {0} is registered twice")]
    Duplicate(CheckId),
    #[error("doctor check {check} requires {missing}, which is not registered")]
    UnknownRequirement { check: CheckId, missing: CheckId },
    #[error("doctor checks require each other in a cycle: {}", ids(.0))]
    Cycle(Vec<CheckId>),
}

fn ids(list: &[CheckId]) -> String {
    list.iter().map(|id| id.0).collect::<Vec<_>>().join(", ")
}

/// Runs checks in prerequisite order, each on its own thread with a
/// deadline. A check that misses its deadline is reported as failed and told
/// to stop ([`CheckContext::is_cancelled`]); the runner does not wait for it.
pub struct Runner {
    checks: Vec<Arc<dyn Check>>,
    timeout: Duration,
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl Runner {
    /// Per-check deadline unless a check overrides it.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

    #[must_use]
    pub fn new() -> Self {
        Self {
            checks: Vec::new(),
            timeout: Self::DEFAULT_TIMEOUT,
        }
    }

    /// Set the default per-check deadline.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Add a check. Registration order breaks ties between checks whose
    /// prerequisites are equally satisfied.
    pub fn register(&mut self, check: impl Check + 'static) -> &mut Self {
        self.checks.push(Arc::new(check));
        self
    }

    /// Execution order (indices into the registration list): every check
    /// after its prerequisites, otherwise in registration order.
    fn order(&self) -> Result<Vec<usize>, RegistryError> {
        let mut index = HashMap::new();
        for (i, check) in self.checks.iter().enumerate() {
            if index.insert(check.id(), i).is_some() {
                return Err(RegistryError::Duplicate(check.id()));
            }
        }
        let mut pending = vec![0usize; self.checks.len()];
        let mut dependents = vec![Vec::new(); self.checks.len()];
        for (i, check) in self.checks.iter().enumerate() {
            for &req in check.requires() {
                let &j = index.get(&req).ok_or(RegistryError::UnknownRequirement {
                    check: check.id(),
                    missing: req,
                })?;
                pending[i] += 1;
                dependents[j].push(i);
            }
        }
        let mut ready: BTreeSet<usize> = (0..self.checks.len())
            .filter(|&i| pending[i] == 0)
            .collect();
        let mut order = Vec::with_capacity(self.checks.len());
        while let Some(i) = ready.pop_first() {
            order.push(i);
            for &d in &dependents[i] {
                pending[d] -= 1;
                if pending[d] == 0 {
                    ready.insert(d);
                }
            }
        }
        if order.len() < self.checks.len() {
            let cyclic = (0..self.checks.len())
                .filter(|&i| pending[i] > 0)
                .map(|i| self.checks[i].id())
                .collect();
            return Err(RegistryError::Cycle(cyclic));
        }
        Ok(order)
    }

    /// Run every check and collect the report. Dependents of a check that
    /// failed or was skipped are skipped, not run.
    pub fn run(&self) -> Result<Report, RegistryError> {
        let order = self.order()?;
        let mut outcome: HashMap<CheckId, Status> = HashMap::new();
        let mut entries = Vec::with_capacity(order.len());
        for i in order {
            let check = &self.checks[i];
            let blocked = check.requires().iter().find_map(|req| match outcome[req] {
                Status::Fail => Some(format!("skipped because {req} failed")),
                Status::Skip => Some(format!("skipped because {req} was skipped")),
                Status::Pass | Status::Warn => None,
            });
            let result = match blocked {
                Some(why) => CheckResult::skip(why),
                None => run_one(check, check.timeout().unwrap_or(self.timeout)),
            };
            outcome.insert(check.id(), result.status);
            entries.push(Entry {
                id: check.id(),
                title: check.title().to_owned(),
                result,
            });
        }
        Ok(Report { entries })
    }
}

fn run_one(check: &Arc<dyn Check>, timeout: Duration) -> CheckResult {
    let ctx = CheckContext::new(timeout);
    let started = Instant::now();
    let (tx, rx) = mpsc::channel();
    let worker = {
        let (check, ctx) = (Arc::clone(check), ctx.clone());
        thread::Builder::new()
            .name(format!("doctor:{}", check.id()))
            .spawn(move || {
                let _ = tx.send(check.run(&ctx));
            })
    };
    let mut result = match worker {
        Err(e) => CheckResult::fail(
            format!("could not start the check: {e}"),
            "The host is out of threads or memory; free resources and re-run.",
        ),
        Ok(worker) => match rx.recv_timeout(timeout) {
            Ok(result) => {
                let _ = worker.join();
                result
            }
            Err(RecvTimeoutError::Timeout) => {
                ctx.cancel();
                CheckResult::fail(
                    format!("timed out after {timeout:?}"),
                    "Something this check waits on did not answer in time. Re-run; if it \
                     keeps timing out, look for a player or network path that is not \
                     responding.",
                )
            }
            Err(RecvTimeoutError::Disconnected) => {
                let why = worker.join().err().map_or_else(String::new, |p| {
                    p.downcast_ref::<&str>()
                        .map(ToString::to_string)
                        .or_else(|| p.downcast_ref::<String>().cloned())
                        .unwrap_or_default()
                });
                CheckResult::fail(
                    format!("the check crashed: {why}"),
                    "This is a FrankenSonos bug; please report it with `fsonos doctor --json` \
                     output.",
                )
            }
        },
    };
    result.duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::{EXIT_FAIL, EXIT_OK, EXIT_WARN};
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};

    enum Behavior {
        Return(CheckResult),
        Sleep(Duration),
        Panic,
        /// Spin until cancelled, then record that cancellation was seen.
        AwaitCancel(Arc<AtomicBool>),
    }

    struct Fake {
        id: CheckId,
        requires: Vec<CheckId>,
        timeout: Option<Duration>,
        behavior: Behavior,
        ran: Arc<Mutex<Vec<CheckId>>>,
    }

    impl Check for Fake {
        fn id(&self) -> CheckId {
            self.id
        }
        fn title(&self) -> &str {
            self.id.0
        }
        fn requires(&self) -> &[CheckId] {
            &self.requires
        }
        fn timeout(&self) -> Option<Duration> {
            self.timeout
        }
        fn run(&self, ctx: &CheckContext) -> CheckResult {
            self.ran.lock().unwrap().push(self.id);
            match &self.behavior {
                Behavior::Return(r) => r.clone(),
                Behavior::Sleep(d) => {
                    thread::sleep(*d);
                    CheckResult::pass("slept")
                }
                Behavior::Panic => panic!("boom"),
                Behavior::AwaitCancel(seen) => {
                    while !ctx.is_cancelled() {
                        thread::sleep(Duration::from_millis(5));
                    }
                    seen.store(true, Ordering::Release);
                    CheckResult::pass("cancelled")
                }
            }
        }
    }

    struct Harness {
        runner: Runner,
        ran: Arc<Mutex<Vec<CheckId>>>,
    }

    impl Harness {
        fn new() -> Self {
            Self {
                runner: Runner::new(),
                ran: Arc::default(),
            }
        }

        fn add(&mut self, id: &'static str, requires: &[&'static str], behavior: Behavior) {
            self.runner.register(Fake {
                id: CheckId(id),
                requires: requires.iter().map(|r| CheckId(r)).collect(),
                timeout: None,
                behavior,
                ran: Arc::clone(&self.ran),
            });
        }

        fn ran(&self) -> Vec<&'static str> {
            self.ran.lock().unwrap().iter().map(|id| id.0).collect()
        }
    }

    fn pass() -> Behavior {
        Behavior::Return(CheckResult::pass("ok"))
    }

    fn statuses(report: &Report) -> Vec<(&'static str, Status)> {
        report
            .entries
            .iter()
            .map(|e| (e.id.0, e.result.status))
            .collect()
    }

    #[test]
    fn prerequisites_run_first_otherwise_registration_order() {
        let mut h = Harness::new();
        h.add("c", &["a", "b"], pass());
        h.add("a", &[], pass());
        h.add("z", &[], pass());
        h.add("b", &["a"], pass());
        let report = h.runner.run().unwrap();
        assert_eq!(h.ran(), ["a", "z", "b", "c"]);
        assert_eq!(
            report.entries.iter().map(|e| e.id.0).collect::<Vec<_>>(),
            ["a", "z", "b", "c"]
        );
    }

    #[test]
    fn dependents_of_a_failure_are_skipped_transitively() {
        let mut h = Harness::new();
        h.add(
            "a",
            &[],
            Behavior::Return(CheckResult::fail("down", "fix it")),
        );
        h.add("b", &["a"], pass());
        h.add("c", &["b"], pass());
        h.add("d", &[], pass());
        let report = h.runner.run().unwrap();
        assert_eq!(h.ran(), ["a", "d"]);
        assert_eq!(
            statuses(&report),
            [
                ("a", Status::Fail),
                ("b", Status::Skip),
                ("c", Status::Skip),
                ("d", Status::Pass),
            ]
        );
        let b = report.get(CheckId("b")).unwrap();
        assert_eq!(b.summary, "skipped because a failed");
        assert_eq!(
            report.get(CheckId("c")).unwrap().summary,
            "skipped because b was skipped"
        );
        assert_eq!(report.exit_code(), EXIT_FAIL);
    }

    #[test]
    fn a_warning_prerequisite_does_not_block() {
        let mut h = Harness::new();
        h.add("a", &[], Behavior::Return(CheckResult::warn("meh", "tune")));
        h.add("b", &["a"], pass());
        let report = h.runner.run().unwrap();
        assert_eq!(h.ran(), ["a", "b"]);
        assert_eq!(report.exit_code(), EXIT_WARN);
    }

    #[test]
    fn self_skip_alone_is_ok() {
        let mut h = Harness::new();
        h.add(
            "s1",
            &[],
            Behavior::Return(CheckResult::skip("no S1 household")),
        );
        h.add("s1.more", &["s1"], pass());
        let report = h.runner.run().unwrap();
        assert_eq!(
            statuses(&report),
            [("s1", Status::Skip), ("s1.more", Status::Skip)]
        );
        assert_eq!(report.exit_code(), EXIT_OK);
    }

    #[test]
    fn timeout_fails_promptly_and_cancels_the_check() {
        let seen = Arc::new(AtomicBool::new(false));
        let mut h = Harness::new();
        h.runner = Runner::new().with_timeout(Duration::from_millis(100));
        h.add("slow", &[], Behavior::Sleep(Duration::from_secs(5)));
        h.add("polite", &[], Behavior::AwaitCancel(Arc::clone(&seen)));
        h.add("after", &["slow"], pass());
        let started = Instant::now();
        let report = h.runner.run().unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "{:?}",
            started.elapsed()
        );
        let slow = report.get(CheckId("slow")).unwrap();
        assert_eq!(slow.status, Status::Fail);
        assert_eq!(slow.summary, "timed out after 100ms");
        assert!(slow.remedy.is_some());
        assert!(slow.duration_ms >= 100);
        assert_eq!(report.get(CheckId("after")).unwrap().status, Status::Skip);
        // The abandoned check was told to stop and noticed.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !seen.load(Ordering::Acquire) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert!(seen.load(Ordering::Acquire), "check observed cancellation");
    }

    #[test]
    fn a_check_can_extend_its_own_deadline() {
        let ran = Arc::default();
        let mut runner = Runner::new().with_timeout(Duration::from_millis(20));
        runner.register(Fake {
            id: CheckId("patient"),
            requires: Vec::new(),
            timeout: Some(Duration::from_secs(5)),
            behavior: Behavior::Sleep(Duration::from_millis(80)),
            ran,
        });
        let report = runner.run().unwrap();
        let r = report.get(CheckId("patient")).unwrap();
        assert_eq!(r.status, Status::Pass);
        assert!(r.duration_ms >= 80);
    }

    #[test]
    fn a_panicking_check_fails_without_taking_down_the_run() {
        let mut h = Harness::new();
        h.add("bad", &[], Behavior::Panic);
        h.add("good", &[], pass());
        let report = h.runner.run().unwrap();
        let bad = report.get(CheckId("bad")).unwrap();
        assert_eq!(bad.status, Status::Fail);
        assert_eq!(bad.summary, "the check crashed: boom");
        assert_eq!(report.get(CheckId("good")).unwrap().status, Status::Pass);
    }

    #[test]
    fn registry_errors() {
        let mut h = Harness::new();
        h.add("a", &[], pass());
        h.add("a", &[], pass());
        assert_eq!(h.runner.run(), Err(RegistryError::Duplicate(CheckId("a"))));

        let mut h = Harness::new();
        h.add("a", &["ghost"], pass());
        assert_eq!(
            h.runner.run(),
            Err(RegistryError::UnknownRequirement {
                check: CheckId("a"),
                missing: CheckId("ghost"),
            })
        );

        let mut h = Harness::new();
        h.add("free", &[], pass());
        h.add("x", &["y"], pass());
        h.add("y", &["x"], pass());
        let err = h.runner.run().unwrap_err();
        assert_eq!(err, RegistryError::Cycle(vec![CheckId("x"), CheckId("y")]));
        assert_eq!(
            err.to_string(),
            "doctor checks require each other in a cycle: x, y"
        );
        assert_eq!(
            h.ran(),
            [] as [&str; 0],
            "nothing runs when the registry is invalid"
        );
    }

    #[test]
    fn empty_registry_is_an_empty_passing_report() {
        let report = Runner::new().run().unwrap();
        assert_eq!(report.entries, [] as [crate::doctor::Entry; 0]);
        assert_eq!(report.exit_code(), EXIT_OK);
    }
}
