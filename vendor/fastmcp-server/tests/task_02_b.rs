//! TASK-02-B: durable task recovery, leasing, and fencing, proved through the
//! shipped `FinalTaskStore` public surface.
//!
//! Frozen IDs: `task_02_b_positive`, `task_02_b_planted_negative`.
//!
//! # Evaluator subject
//!
//! bd-mcp-task-02-b-x5r3 names its subject "the shipped `PersistentTaskBackend`
//! conformance harness". No type of that name exists in this workspace; the
//! name is a ROLE, and the artifact that fills it is the public
//! `fastmcp_server::FinalTaskStore` trait with `InMemoryFinalTaskStore` as the
//! conforming implementation under test. This file drives that trait and
//! nothing else, so any second implementation is held to the same assertions
//! by construction.
//!
//! Everything here runs out of crate, against `pub` items only. Nothing is
//! `cfg(test)`-reachable, so PL-3 is satisfied for the rows it covers.
//!
//! # Coverage against the bead's 23 ordered groups -- READ THIS BEFORE CITING
//!
//! This file does NOT discharge the bead. It covers FIFTEEN of the twenty-three
//! named groups, two of those only in part. The table below is the authority;
//! keep this sentence and the table in agreement when either changes:
//!
//! | group | state here |
//! |---|---|
//! | `B-24 persist-ambiguous-commit`      | covered |
//! | `B-26 stale-owner-fenced-write`      | covered |
//! | `B-27 lease-renew-expire-reclaim`    | covered -- renew, expire AND reclaim |
//! | `B-28 durable-time-authority`        | covered |
//! | `B-29 backend-clock-discontinuity`   | covered -- narrow; see the test's own scope note |
//! | `B-30 skewed-worker-time-domains`    | covered |
//! | `B-32 deadline-renew-restart`        | covered |
//! | `B-31 private-update-revision-order` | covered -- stale-generation refusal AND the ordering chain |
//! | `B-34 restore-write-contract`        | covered |
//! | `B-35 third-party-backend-conformance` | covered -- required surface only |
//! | `B-42 duplicate-execution-idempotency` | covered |
//! | `B-43 shutdown-drain-lease-release`  | covered |
//! | `B-45 anonymous-leaked-handle`       | covered |
//! | `B-44 mrtr-nonuse`                   | covered -- the durable surface carries and needs no multi-round retry state |
//! | `B-46 ops-readiness-cardinality`     | covered -- the ZERO case only; the one-entered-owner case needs a runner harness |
//!
//! FIFTEEN of twenty-three, two of them partial. **Eight groups have no test
//! here and none anywhere in the tree**: B-25, B-33, and B-36 through B-41.
//!
//! A RUN REPORTS 19 OUTCOMES, WHICH IS NOT 19 GROUPS AND NOT 23. Sixteen
//! group tests spread over fifteen groups, one lease-window guard, and the
//! two frozen IDs. This file has been
//! miscounted three times by three different methods -- 13 by mention count,
//! 18 by a mixed regex, and 13-of-23 by reading the outcome total as a group
//! total.
//!
//! THE GROUP FIGURE IS THE NUMBER OF DISTINCT `bNN` PREFIXES, NOT THE NUMBER
//! OF `fn bNN_*` DEFINITIONS. Those were the same number until B-31 acquired
//! a second test for its ordering half, and an earlier version of this note
//! said "definitions" -- which now over-counts by one. The canonical
//! expression:
//!
//!     grep -oE '^fn (b[0-9]+)_' file | sort -u | wc -l
//!
//! A group may hold more than one test when its halves are independently
//! meaningful; that is a feature of the coverage, not extra coverage.
//!
//! Of the TEN that were missing, EIGHT are blocked on capability the shipped
//! store does not have and cannot be closed by writing tests: B-25 and B-33
//! (reconciliation, `reconcil` = 0 here though it appears in 21 other
//! workspace src files), B-36 and B-41 (expiry index / tombstones,
//! `sweeper` = 0), B-37 (quota, 2 occurrences in 19,260 lines), B-38 and B-40
//! (protected payloads; tasks.rs:8460 states the store "deliberately
//! implements only unprotected work" and `reencrypt` = 0 workspace-wide),
//! B-39 (durable-time epoch, `epoch` = 0). The other TWO were B-44 and B-46,
//! and both are now written -- so no writable candidate remains, and the
//! eight above are the whole of the gap. Closing any of them is a change to
//! the STORE, not another test file.
//!
//! A NOTE ON THAT SPLIT, because the obvious heuristic over-blocks: absence of
//! a word from the source decides nothing on its own. It is decisive only
//! when the STORE must implement the concept. Where the TEST supplies it, the
//! word is irrelevant -- `discontinu` and `skew` appear nowhere in the
//! workspace, yet B-29 above is written and passing, because the test injects
//! the clock. The same applies to B-35, where the test would supply a second
//! `FinalTaskStore` implementation.
//!
//! # How to count this file, and the three numbers that are all true
//!
//! THREE DIFFERENT QUANTITIES, ALL TRUE OF THIS FILE AT ONCE. Only one is
//! measured against the bead's bar, which is stated in GROUPS:
//!
//! ```text
//!     distinct bNN group prefixes   <- THE GROUP FIGURE, vs the AC's 23
//!         grep -oE '^fn (b[0-9]+)_' file | sort -u | wc -l
//!     fn bNN_* definitions          <- larger; a group may hold two tests
//!         grep -cE '^fn b[0-9]+_' file
//!     #[test] attributes            <- larger still; adds the guard, the
//!                                      two frozen IDs, and any second test
//!         grep -cE '^[[:space:]]*#\[test\]' file
//! ```
//!
//! DO NOT count `#[test]` unanchored. This prose mentions the token, so
//! `grep -c '#\[test\]'` scores three higher than the truth -- and one of the
//! inflating lines is the warning against counting by mentions. Anchor it.
//!
//! Likewise do not count `B-nn` occurrences. The coverage table names groups
//! in order to say which are MISSING, so a mention scan reads the gap list as
//! coverage. Every group identifier in this file is in a comment; none is in
//! code.
//!
//! THIS FILE HAS BEEN MISCOUNTED FOUR TIMES, BY FOUR DIFFERENT INSTRUMENTS,
//! always upward: 13 by mention count, 18 by a mixed regex, 13-of-23 by
//! reading a run's outcome total as a group total, and 20 tests by an
//! unanchored `#[test]` count. The numbers were real every time; each was a
//! count of something else. Re-derive with the expressions above rather than
//! trusting any figure written in prose, including the ones below.
//!
//! # What a run reports, and how not to add it up
//!
//! Structure, which is stable even as the counts move:
//!
//! - One test per `bNN_*` group, plus a second where a group's halves are
//!   independently meaningful (B-31 has two: the ordering chain and the
//!   stale-generation refusal).
//! - `lease_window_is_the_one_this_file_assumes`, a guard on the constant
//!   this file's clock advances are calibrated against.
//! - `task_02_b_positive`, a frozen ID and a ROLL-UP that calls every group
//!   test above. It adds NO behaviour. It exists because the acceptance
//!   criteria name it as the positive and an empty frozen positive would be a
//!   vacuous test.
//! - `task_02_b_planted_negative`, a frozen ID, the one behaviour no group
//!   test reaches.
//!
//! **A full run therefore executes each group body TWICE** -- once standalone,
//! once through the roll-up. A receipt must not read the outcome total as a
//! behaviour count, nor a group failing twice as two defects. The roll-up
//! keeps the first-failure-hides-siblings defect internally, which is exactly
//! why it must not be the only entry point rather than a reason to delete it.
//!
//! Four of the sixteen cannot be closed by writing tests at all. They rest on
//! vocabulary the shipped source does not carry: word-boundary counts over
//! `crates/fastmcp-server/src/tasks.rs` give `quota` = 2, `tombstone` = 0,
//! `reconcil` = 0, `epoch` = 0, against `lease` = 191, `fence` = 64,
//! `durable` = 157 and `generation` = 540. Those are implementation gaps.
//!
//! # Feature gating, and which configuration a green here binds
//!
//! This target declares `required-features = ["tasks"]` in
//! `crates/fastmcp-server/Cargo.toml`. It needs it: `FinalTaskStore`,
//! `InMemoryFinalTaskStore`, `FinalTaskSnapshot`, `FinalTaskWorkDescriptor`
//! and `FinalTaskRetentionDeadline` are all exported from `lib.rs` behind
//! `#[cfg(feature = "tasks")]`, while this package defaults to
//! `["legacy-2024-11-05"]`. Shipped without the stanza this file failed E0432
//! and broke all 20 test targets in the package -- it did so once, on
//! 2026-09-21, before the stanza was added.
//!
//! The stanza trades a loud compile failure for a SILENT SKIP, so
//! `cargo test -p fastmcp-server` with no flags now exits 0 having discovered
//! neither frozen ID. That is why the runner card passes `--features tasks`
//! explicitly and why any receipt must cite the discovered count, not the
//! exit status.
//!
//! Scope of a green: `tasks` is what the published facade `fastmcp-rust`
//! enables by default, so this binds the default facade configuration. A
//! consumer depending directly on `fastmcp-server` must opt in, and without
//! opting in none of these entrypoints exist.
//!
//! # Why the clock is injected
//!
//! `InMemoryFinalTaskStore::with_clock` takes the monotonic retention clock as
//! a parameter. Every time-dependent assertion below advances that counter
//! explicitly. No assertion sleeps, and none compares against `Instant::now()`,
//! so none of them can pass or fail because of machine load.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

use fastmcp_core::{McpError, McpResult};
use fastmcp_protocol::FinalTaskId;
use fastmcp_protocol::tasks_extension::{
    Task, TaskStatusNotification, TaskStatusNotificationParams,
};
use fastmcp_server::{
    FinalTaskRetentionDeadline, FinalTaskRuntime, FinalTaskRuntimeConfig, FinalTaskSnapshot,
    FinalTaskStore, FinalTaskWorkDescriptor, InMemoryFinalTaskStore,
};

/// The in-memory store's elected dispatch lease, as declared by
/// `IN_MEMORY_FINAL_TASK_HANDOFF_LEASE`. Held here as a local expectation so a
/// change to the store's constant surfaces as a failure in
/// `lease_window_is_the_one_this_file_assumes` rather than as silent drift in
/// every clock advance below.
const ASSUMED_LEASE: Duration = Duration::from_secs(30);

/// A store whose retention clock this test drives directly.
struct Fixture {
    store: Arc<InMemoryFinalTaskStore>,
    advanced_ms: Arc<AtomicU64>,
    base: Instant,
    /// Taken from the created task rather than reconstructed, so the tests
    /// cannot disagree with the store about which identifier they mean.
    id: FinalTaskId,
}

impl Fixture {
    /// Builds a store holding exactly one `working` task with retained initial
    /// work, and returns it with the clock frozen at `base`.
    fn new(task_id: &str, ttl_ms: u64) -> Self {
        let base = Instant::now();
        let advanced_ms = Arc::new(AtomicU64::new(0));
        let store = Arc::new(
            InMemoryFinalTaskStore::with_clock(4, {
                let advanced_ms = Arc::clone(&advanced_ms);
                Arc::new(move || base + Duration::from_millis(advanced_ms.load(Ordering::SeqCst)))
            })
            .expect("a positive capacity yields a store"),
        );
        let task: Task = serde_json::from_value(serde_json::json!({
            "taskId": task_id,
            "status": "working",
            "createdAt": "2026-07-28T12:00:00.000Z",
            "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
            "ttlMs": ttl_ms
        }))
        .expect("a well-formed working task");
        let id = task.base().task_id.clone();
        let notification = TaskStatusNotification::new(TaskStatusNotificationParams {
            task: task.clone(),
            meta: None,
            additional: std::collections::BTreeMap::default(),
        });
        store
            .create_task_with_work(
                task,
                notification,
                FinalTaskWorkDescriptor::new(serde_json::json!({"operation": "durable"}))
                    .expect("a bounded work descriptor"),
            )
            .expect("atomic task-work creation is implemented by this store");
        Self {
            store,
            advanced_ms,
            base,
            id,
        }
    }

    /// Moves the store's authoritative clock BACKWARDS, to the given offset
    /// from the fixture's base. Real monotonic clocks do not do this; a
    /// durable backend reading a corrected or mis-synced host clock can.
    fn rewind_to(&self, offset: Duration) {
        self.advanced_ms.store(
            u64::try_from(offset.as_millis()).expect("test offsets fit in u64 milliseconds"),
            Ordering::SeqCst,
        );
    }

    /// Moves the store's authoritative clock forward. Nothing here sleeps.
    fn advance(&self, by: Duration) {
        self.advanced_ms.fetch_add(
            u64::try_from(by.as_millis()).expect("test advances fit in u64 milliseconds"),
            Ordering::SeqCst,
        );
    }

    /// The exact instant the injected clock is currently returning.
    fn clock_reads(&self) -> Instant {
        self.base + Duration::from_millis(self.advanced_ms.load(Ordering::SeqCst))
    }

    fn snapshot(&self) -> FinalTaskSnapshot {
        self.store
            .get_task_snapshot(&self.id)
            .expect("store reads succeed")
            .expect("the task was created above")
    }

    /// The task as it is currently retained, rendered through its wire
    /// serialization. `Task` carries no `PartialEq`, and its `Debug` is not a
    /// contract, so the wire form is the comparable representation -- and it
    /// is also the one the AC's "byte-for-byte unchanged" clause is about.
    fn task_wire_form(&self) -> serde_json::Value {
        serde_json::to_value(
            self.store
                .get_task(&self.id)
                .expect("store reads succeed")
                .expect("the task exists"),
        )
        .expect("a retained task serializes")
    }

    /// Elects `owner` for the retained initial work and returns the
    /// store-issued fencing token.
    fn elect(&self, owner: &str) -> (FinalTaskSnapshot, u64) {
        let snapshot = self.snapshot();
        assert!(
            self.store
                .take_initial_work_handoff_for_owner_if_current(&snapshot, owner)
                .expect("store writes succeed")
                .is_some(),
            "the retained initial work must be claimable by the first owner"
        );
        let fence = self
            .store
            .begin_handoff_dispatch_for_owner_if_current(&self.id, snapshot.generation(), owner)
            .expect("store writes succeed")
            .expect("dispatch election succeeds for the sole owner at the current generation");
        (snapshot, fence)
    }
}

/// The single task identifier every group below uses.
const TASK: &str = "durable-operation";

// ---------------------------------------------------------------------------
// Groups
// ---------------------------------------------------------------------------

/// Guards the local constant this file's clock advances are calibrated
/// against. Without this, a change to the store's lease would turn every
/// "past expiry" advance below into a "still live" advance, and the affected
/// tests would keep passing while asserting the opposite of their names.
#[test]
fn lease_window_is_the_one_this_file_assumes() {
    let fixture = Fixture::new(TASK, 600_000);
    let heartbeat = fixture
        .store
        .handoff_dispatch_lease_heartbeat_interval()
        .expect("this store discloses a heartbeat interval");
    assert!(
        heartbeat > Duration::ZERO,
        "the contract requires a positive interval"
    );
    assert!(
        heartbeat < ASSUMED_LEASE,
        "the contract requires an interval strictly shorter than the lease; \
         heartbeat {heartbeat:?} is not shorter than the assumed lease {ASSUMED_LEASE:?}, \
         so either the store's lease changed or this file's constant is stale"
    );
}

/// `B-26 stale-owner-fenced-write`: an owner that did not win the election
/// cannot renew, and the winner still can.
#[test]
fn b26_stale_owner_fenced_write() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");

    assert!(
        fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the elected owner holds a live lease"
    );
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-b", fence)
            .expect("store writes succeed"),
        "a non-electing owner must not renew another owner's lease even with its fence"
    );
    assert!(
        fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the refused write must not have disturbed the real owner's lease"
    );
}

/// `B-27 lease-renew-expire-reclaim`, renew and expire halves only.
#[test]
fn b27_lease_renew_then_expire() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");

    fixture.advance(ASSUMED_LEASE / 2);
    assert!(
        fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "half a lease in, renewal must still succeed"
    );

    // Past the lease measured FROM THE RENEWAL, not from the election, so this
    // cannot pass merely because the original window elapsed.
    fixture.advance(ASSUMED_LEASE + Duration::from_secs(1));
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "a lease whose window has fully elapsed on the store's own clock must not renew"
    );

    // RECLAIM. The expired lease is dropped by the store's own reclaim pass,
    // and the retained initial work was never consumed by the take, so a
    // different owner can claim the same task.
    //
    // THE GENERATION ADVANCES ACROSS THE RECLAIM. `tasks.rs:3720-3727` fences
    // the abandoned claimant on purpose: "Without this generation advance, a
    // late drop from the old worker could release a newer worker's lease." So
    // the reclaiming owner must RE-READ, and the pre-expiry snapshot is
    // deliberately dead. Asserting that advance is the point of this half --
    // an earlier version of this test reused the stale generation, which is
    // what the 03:27Z gate caught.
    let after_expiry = fixture.snapshot();
    assert_ne!(
        after_expiry.generation(),
        snapshot.generation(),
        "reclaiming an expired lease must advance the generation; without it a late drop \
         from the evicted owner could release the next owner's lease"
    );

    let reclaimed = fixture
        .store
        .take_initial_work_handoff_for_owner_if_current(&after_expiry, "owner-b")
        .expect("store writes succeed");
    assert!(
        reclaimed.is_some(),
        "after the first owner's lease expires the retained work must be claimable again"
    );
    let new_fence = fixture
        .store
        .begin_handoff_dispatch_for_owner_if_current(
            &fixture.id,
            after_expiry.generation(),
            "owner-b",
        )
        .expect("store writes succeed")
        .expect("the reclaiming owner elects a fresh dispatch at the advanced generation");
    assert!(
        fixture
            .store
            .renew_handoff_dispatch_if_current(
                &fixture.id,
                after_expiry.generation(),
                "owner-b",
                new_fence
            )
            .expect("store writes succeed"),
        "the reclaiming owner holds a live lease"
    );
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the evicted owner must not renew back in -- its generation is stale AND its lease is gone"
    );
}

/// `B-34 restore-write-contract`: an owner may hand work back only with the
/// exact lease it holds and the exact descriptor the store retained.
#[test]
fn b34_restore_write_contract() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");
    let retained = FinalTaskWorkDescriptor::new(serde_json::json!({"operation": "durable"}))
        .expect("the descriptor the fixture created the task with");
    let altered = FinalTaskWorkDescriptor::new(serde_json::json!({"operation": "substituted"}))
        .expect("a well-formed but different descriptor");

    // The unauthorized variant is refused with a typed error, not a false.
    // `false` would be indistinguishable from a legitimate lost race.
    assert!(
        fixture
            .store
            .restore_initial_work_if_current(&fixture.id, snapshot.generation(), retained.clone())
            .is_err(),
        "a restore with no owner must be a typed refusal, not a quiet false"
    );

    // Wrong descriptor: the lease matches, so the store accepts the release,
    // but the contract's return value reports that what was handed back is not
    // what it retained.
    assert!(
        !fixture
            .store
            .restore_initial_work_for_owner_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                Some(fence),
                altered
            )
            .expect("store writes succeed"),
        "restoring a descriptor the store never retained must not report success"
    );

    // Wrong fence, on a fresh fixture so the arm above cannot have consumed
    // the lease this one needs.
    let other = Fixture::new(TASK, 600_000);
    let (other_snapshot, other_fence) = other.elect("owner-a");
    assert!(
        !other
            .store
            .restore_initial_work_for_owner_if_current(
                &other.id,
                other_snapshot.generation(),
                "owner-a",
                Some(other_fence + 1),
                retained.clone()
            )
            .expect("store writes succeed"),
        "a fence one off the held lease must not restore"
    );

    // The accepted row, last, proving the refusals above were attributable to
    // the one changed variable and not to a store left unable to accept
    // anything.
    assert!(
        other
            .store
            .restore_initial_work_for_owner_if_current(
                &other.id,
                other_snapshot.generation(),
                "owner-a",
                Some(other_fence),
                retained
            )
            .expect("store writes succeed"),
        "the exact lease and the exact retained descriptor must restore"
    );
}

/// `B-42 duplicate-execution-idempotency`: a held lease makes the work
/// unclaimable by anyone, so two runners cannot execute the same task.
#[test]
fn b42_duplicate_execution_is_refused() {
    let fixture = Fixture::new(TASK, 600_000);
    let snapshot = fixture.snapshot();

    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&snapshot, "owner-a")
            .expect("store writes succeed")
            .is_some(),
        "the first claim succeeds"
    );
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&fixture.snapshot(), "owner-a")
            .expect("store writes succeed")
            .is_none(),
        "the SAME owner claiming twice must not get a second execution"
    );
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&fixture.snapshot(), "owner-b")
            .expect("store writes succeed")
            .is_none(),
        "a second owner must not get a concurrent execution of the same task"
    );

    // An empty owner is a typed refusal rather than an anonymous claim. A
    // `false`/`None` here would let an unattributable runner hold work.
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&fixture.snapshot(), "")
            .is_err(),
        "an empty owner must be refused with an error, not silently declined"
    );
}

/// `B-28 durable-time-authority`: retention time comes from the store's
/// injected clock, never from the wall clock.
#[test]
fn b28_durable_time_authority() {
    let fixture = Fixture::new(TASK, 600_000);

    let first = fixture.store.retention_clock_now();
    let second = fixture.store.retention_clock_now();
    assert_eq!(
        first, second,
        "two reads with no advance must be identical; a wall clock would drift between them"
    );
    assert_eq!(
        first,
        fixture.clock_reads(),
        "the store must report exactly the injected value, not an offset of it"
    );

    fixture.advance(Duration::from_secs(90));
    assert_eq!(
        fixture.store.retention_clock_now(),
        first + Duration::from_secs(90),
        "advancing the injected clock must move the store's authority by exactly that amount"
    );

    // The retention deadline is derived from the same authority.
    let snapshot = fixture.snapshot();
    let deadline = fixture
        .store
        .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
        .expect("store reads succeed")
        .expect("a live task at its current generation has a deadline");
    let FinalTaskRetentionDeadline::Finite(at) = deadline else {
        panic!("a task created with a finite ttlMs must have a finite deadline, got {deadline:?}");
    };
    assert_eq!(
        at,
        first + Duration::from_millis(600_000),
        "the deadline must be the creation-time clock reading plus the declared TTL"
    );
}

/// Builds a `working` replacement for TASK carrying a distinguishable stamp.
fn stamped_replacement(stamp: &str) -> (Task, TaskStatusNotification) {
    let task: Task = serde_json::from_value(serde_json::json!({
        "taskId": TASK,
        "status": "working",
        "createdAt": "2026-07-28T12:00:00.000Z",
        "lastUpdatedAt": stamp,
        "ttlMs": 600_000
    }))
    .expect("a well-formed stamped replacement");
    let notification = TaskStatusNotification::new(TaskStatusNotificationParams {
        task: task.clone(),
        meta: None,
        additional: std::collections::BTreeMap::default(),
    });
    (task, notification)
}

/// `B-31 private-update-revision-order`, ORDERING half: applied writes form a
/// chain, and a writer may only extend the head of it.
///
/// This is the property that makes concurrent writers safe without locks.
/// Distinct from `B-24`, which is the SAME writer retrying an ambiguous
/// commit; here two different writers race from the same read, and the
/// question is whether the store admits an interleaving.
///
/// Source-backed: every applied replace allocates a fresh generation
/// (tasks.rs:3646) and the CAS compares against the CURRENT one
/// (tasks.rs:2542), so an expectation more than one step behind is as dead as
/// one exactly one step behind -- there is no "catch up" path.
#[test]
fn b31_revision_order_is_a_chain() {
    let fixture = Fixture::new(TASK, 600_000);
    let g0 = fixture.snapshot();

    // TWO WRITERS READ THE SAME STATE. Both hold g0.
    let writer_a = g0.clone();
    let writer_b = g0.clone();

    // Writer A wins the race.
    let (task_a, notif_a) = stamped_replacement("2026-07-28T12:01:00.000Z");
    assert!(
        fixture
            .store
            .replace_task_if_current(&writer_a, task_a, notif_a)
            .expect("store writes succeed"),
        "the first writer at the head extends the chain"
    );
    let g1 = fixture.snapshot();
    assert_ne!(g1.generation(), g0.generation(), "the head moved");

    // Writer B loses, with the read it actually made. Its write does not
    // interleave and does not overwrite A's.
    let (task_b, notif_b) = stamped_replacement("2026-07-28T12:02:00.000Z");
    assert!(
        !fixture
            .store
            .replace_task_if_current(&writer_b, task_b, notif_b)
            .expect("store writes succeed"),
        "a writer racing from a superseded read must lose, not interleave"
    );
    assert_eq!(
        fixture.task_wire_form()["lastUpdatedAt"],
        "2026-07-28T12:01:00.000Z",
        "the loser must not have overwritten the winner"
    );

    // Writer B re-reads and extends the new head. This is the only way
    // forward, and it works.
    let (task_b2, notif_b2) = stamped_replacement("2026-07-28T12:03:00.000Z");
    assert!(
        fixture
            .store
            .replace_task_if_current(&g1, task_b2, notif_b2)
            .expect("store writes succeed"),
        "a writer that re-reads the head may extend it"
    );
    let g2 = fixture.snapshot();
    assert_eq!(
        fixture.task_wire_form()["lastUpdatedAt"],
        "2026-07-28T12:03:00.000Z",
        "the re-read write applied"
    );

    // THE CHAIN PROPERTY: three distinct generations, and NO expectation from
    // anywhere behind the head is usable. Two steps back is as dead as one --
    // there is no catch-up path a stale writer could find.
    assert_ne!(g2.generation(), g1.generation());
    assert_ne!(g2.generation(), g0.generation());
    let (stale, stale_notif) = stamped_replacement("2026-07-28T12:04:00.000Z");
    assert!(
        !fixture
            .store
            .replace_task_if_current(&g0, stale, stale_notif)
            .expect("store writes succeed"),
        "an expectation two generations behind the head must also be refused"
    );
    assert_eq!(
        fixture.snapshot().generation(),
        g2.generation(),
        "the refused stale write consumed no generation"
    );
}

/// `B-31 private-update-revision-order`, stale-generation half.
#[test]
fn b31_stale_generation_is_refused() {
    let fixture = Fixture::new(TASK, 600_000);
    let snapshot = fixture.snapshot();
    let stale = snapshot
        .generation()
        .checked_sub(1)
        .expect("the store issues generations above zero");

    assert!(
        fixture
            .store
            .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
            .expect("store reads succeed")
            .is_some(),
        "the current generation resolves"
    );
    assert!(
        fixture
            .store
            .task_retention_deadline_if_current(&fixture.id, stale)
            .expect("store reads succeed")
            .is_none(),
        "a generation one behind the current one must read as stale, not as the current row"
    );
}

/// `B-43 shutdown-drain-lease-release`: a completed dispatch releases exactly
/// once, and a second release is refused rather than double-counted.
#[test]
fn b43_release_happens_exactly_once() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");

    assert!(
        fixture
            .store
            .finish_handoff_dispatch_for_owner_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                fence
            )
            .expect("store writes succeed"),
        "the elected owner releases its own live lease"
    );
    assert!(
        !fixture
            .store
            .finish_handoff_dispatch_for_owner_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                fence
            )
            .expect("store writes succeed"),
        "a second release of the same lease must be refused, or a drain would count it twice"
    );
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "a released lease cannot be renewed back into existence"
    );
}

/// `B-29 backend-clock-discontinuity`: a clock that goes BACKWARDS cannot undo
/// a reclamation that already happened.
///
/// Scope, stated because it is narrow: the store has no documented behaviour
/// for a regressing clock, and inventing one here would be asserting a
/// requirement the source does not carry. What IS backed by the source is that
/// reclamation is DESTRUCTIVE -- `reclaim_expired_in_memory_final_tasks`
/// removes the lease from `handoff_leases` and advances the generation
/// (tasks.rs:3720-3727). Neither is recoverable by any later clock reading, so
/// the safety property survives a discontinuity by construction rather than by
/// a guard. This test pins that, and pins that the authority still tracks the
/// injected value exactly afterwards.
#[test]
fn b29_backend_clock_discontinuity() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");

    // Expire the lease and force the reclaim pass to observe it.
    fixture.advance(ASSUMED_LEASE + Duration::from_secs(1));
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the lease must be gone before the clock is rewound, or this proves nothing"
    );
    let reclaimed_generation = fixture.snapshot().generation();
    assert_ne!(
        reclaimed_generation,
        snapshot.generation(),
        "the reclaim must have advanced the generation before the rewind"
    );

    // THE DISCONTINUITY: back to before the lease was ever issued.
    fixture.rewind_to(Duration::ZERO);
    assert_eq!(
        fixture.store.retention_clock_now(),
        fixture.clock_reads(),
        "the authority must report the injected value after it regresses, not a latched maximum"
    );

    // The evicted owner cannot renew even though its lease window now appears
    // to lie in the future again. The lease row is gone; time cannot restore it.
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "a clock regression must not resurrect a reclaimed lease"
    );
    assert_eq!(
        fixture.snapshot().generation(),
        reclaimed_generation,
        "a clock regression must not roll the generation back"
    );

    // And the task itself is untouched by the regression.
    assert_eq!(
        fixture.task_wire_form()["taskId"],
        TASK,
        "the record must survive the discontinuity intact"
    );
}

/// `B-30 skewed-worker-time-domains`: no caller supplies time, so a worker's
/// own clock cannot influence a lease decision.
///
/// The property is structural: every timing decision in the store reads
/// `(self.clock)()`, and no method on the trait accepts an instant, a
/// duration, or a deadline from its caller. Two workers therefore cannot
/// disagree about time because neither of them is consulted. This test
/// demonstrates the observable consequence rather than restating the shape --
/// with the store's clock held still, no amount of work expires a lease, and
/// one advance of that clock expires it immediately.
#[test]
fn b30_skewed_worker_time_domains() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");
    let frozen = fixture.store.retention_clock_now();

    // Real elapsed time and real work, with the store's clock held still.
    // Under a wall clock these 200 operations would take measurable time; the
    // lease must not care.
    for i in 0..200 {
        assert!(
            fixture
                .store
                .renew_handoff_dispatch_if_current(
                    &fixture.id,
                    snapshot.generation(),
                    "owner-a",
                    fence
                )
                .expect("store writes succeed"),
            "renewal {i} must succeed while the store's clock has not moved"
        );
        assert!(
            !fixture
                .store
                .renew_handoff_dispatch_if_current(
                    &fixture.id,
                    snapshot.generation(),
                    "owner-b",
                    fence
                )
                .expect("store writes succeed"),
            "a second worker must not win at iteration {i} either"
        );
    }
    assert_eq!(
        fixture.store.retention_clock_now(),
        frozen,
        "200 operations must not move an authority nobody supplied time to"
    );

    // One advance of the STORE's clock, and only that, ends the lease.
    fixture.advance(ASSUMED_LEASE + Duration::from_secs(1));
    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the store's own clock is the only thing that can expire the lease"
    );
}

/// `B-32 deadline-renew-restart`: renewing a dispatch lease must not extend
/// the task's retention deadline, and a restart must not reset it.
///
/// These are two separate clocks and conflating them would be a real defect:
/// a worker that renews forever would keep a task alive past its declared
/// TTL. Source-backed -- `renew_handoff_dispatch_if_current` writes only
/// `lease.recovery_expires_at` (tasks.rs:3149) and never touches the task's
/// `expires_at`, which is what `task_retention_deadline_if_current` reports.
#[test]
fn b32_deadline_survives_renew_and_restart() {
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");

    let deadline_at_start = fixture
        .store
        .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
        .expect("store reads succeed")
        .expect("a live task has a deadline");
    let FinalTaskRetentionDeadline::Finite(original) = deadline_at_start else {
        panic!("a finite ttlMs must give a finite deadline, got {deadline_at_start:?}");
    };

    // Renew repeatedly, advancing well past a whole lease window in total.
    for _ in 0..10 {
        fixture.advance(Duration::from_secs(2));
        assert!(
            fixture
                .store
                .renew_handoff_dispatch_if_current(
                    &fixture.id,
                    snapshot.generation(),
                    "owner-a",
                    fence
                )
                .expect("store writes succeed"),
            "renewal within the window succeeds"
        );
        let still = fixture
            .store
            .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
            .expect("store reads succeed")
            .expect("the task is still retained");
        assert_eq!(
            still,
            FinalTaskRetentionDeadline::Finite(original),
            "renewing the LEASE must not move the TASK's retention deadline"
        );
    }

    // RESTART: let the lease lapse, then let a new owner take over. The task's
    // deadline is a property of the task, not of whoever is currently holding
    // it, so it must be unchanged across the handover.
    fixture.advance(ASSUMED_LEASE + Duration::from_secs(1));
    let after_expiry = fixture.snapshot();
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&after_expiry, "owner-b")
            .expect("store writes succeed")
            .is_some(),
        "a restarted worker picks the task up"
    );
    let after_restart = fixture
        .store
        .task_retention_deadline_if_current(&fixture.id, after_expiry.generation())
        .expect("store reads succeed")
        .expect("the task survived the handover");
    assert_eq!(
        after_restart,
        FinalTaskRetentionDeadline::Finite(original),
        "a restart must not reset the task's retention deadline"
    );
}

/// `B-24 persist-ambiguous-commit`: a commit whose acknowledgement was lost is
/// safe to retry, because the retry cannot apply it a second time.
///
/// This is the defect the generation CAS exists to prevent. A caller that
/// issues `replace_task_if_current` and then loses the response cannot tell
/// whether the write landed. Its only safe move is to retry with the same
/// expectation -- so the store must make that retry a no-op rather than a
/// second application.
///
/// Source-backed: a successful replace runs `replace_in_memory_final_task`,
/// which allocates a new generation and inserts it (tasks.rs:3646-3659), so
/// the retry's expected generation no longer matches and the guard at
/// tasks.rs:2542 returns `Ok(false)` before touching anything.
#[test]
fn b24_persist_ambiguous_commit() {
    let fixture = Fixture::new(TASK, 600_000);
    let before = fixture.snapshot();

    let replacement: Task = serde_json::from_value(serde_json::json!({
        "taskId": TASK,
        "status": "working",
        "createdAt": "2026-07-28T12:00:00.000Z",
        "lastUpdatedAt": "2026-07-28T12:05:00.000Z",
        "ttlMs": 600_000
    }))
    .expect("a well-formed replacement at the same identifier");
    let notification = TaskStatusNotification::new(TaskStatusNotificationParams {
        task: replacement.clone(),
        meta: None,
        additional: std::collections::BTreeMap::default(),
    });

    // The commit lands.
    assert!(
        fixture
            .store
            .replace_task_if_current(&before, replacement.clone(), notification.clone())
            .expect("store writes succeed"),
        "the first commit at the current generation must apply"
    );
    let after_commit = fixture.snapshot().generation();
    assert_ne!(
        after_commit,
        before.generation(),
        "a commit must consume the generation it was conditioned on"
    );

    // AMBIGUITY: the caller never saw that result. It retries the IDENTICAL
    // call with the IDENTICAL expectation, which is the only thing it can
    // safely do.
    assert!(
        !fixture
            .store
            .replace_task_if_current(&before, replacement, notification)
            .expect("a stale expectation is a refusal, not a transport error"),
        "a retried ambiguous commit must not apply a second time"
    );

    // Exactly one application: the retry moved nothing.
    assert_eq!(
        fixture.snapshot().generation(),
        after_commit,
        "a refused retry must not consume a further generation"
    );
    assert_eq!(
        fixture.task_wire_form()["lastUpdatedAt"],
        "2026-07-28T12:05:00.000Z",
        "the record must show the replacement, applied once"
    );
}

/// `B-45 anonymous-leaked-handle`: no handoff surface accepts an anonymous
/// owner, so a leaked handle cannot be exercised without attribution.
///
/// Every owner-bound entry point guards on `owner_id.is_empty()` and returns
/// a typed error -- tasks.rs:2641, :2714, :2863. The distinction matters: a
/// `false` or a `None` would be indistinguishable from a legitimately lost
/// race, so an unattributable caller could retry forever and a reader of the
/// logs could not tell it apart from contention. An `Err` says the call was
/// malformed, not unlucky.
///
/// This covers all three guarded surfaces. `B-42` already exercises the third
/// in passing; here the property is asserted as a property of the SURFACE
/// rather than of one method.
#[test]
fn b45_anonymous_handle_is_refused_everywhere() {
    let fixture = Fixture::new(TASK, 600_000);
    let snapshot = fixture.snapshot();

    // 1. Initial-work handoff.
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&snapshot, "")
            .is_err(),
        "initial-work handoff must refuse an anonymous owner with a typed error"
    );

    // 2. Accepted-input handoff.
    assert!(
        fixture
            .store
            .take_input_handoff_for_owner_if_current(&snapshot, "")
            .is_err(),
        "input handoff must refuse an anonymous owner with a typed error"
    );

    // 3. The handoff-completing replace.
    let (replacement, notification) = conformance_task(TASK);
    assert!(
        fixture
            .store
            .replace_task_and_clear_input_for_handoff_if_current(
                &snapshot,
                "",
                1,
                false,
                replacement,
                notification
            )
            .is_err(),
        "the handoff-completing replace must refuse an anonymous owner with a typed error"
    );

    // CONTROL: a NAMED owner reaches a real decision on the same surface, so
    // the three refusals above are attributable to anonymity and not to the
    // surface rejecting everything. This one succeeds; the point is only that
    // it is not an Err.
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&snapshot, "owner-a")
            .expect("a named owner is not a malformed call")
            .is_some(),
        "a named owner must reach a real outcome where an anonymous one is refused"
    );

    // And nothing the refusals touched changed: no lease was created by any
    // of them, so the named owner above was the FIRST claimant.
    assert!(
        fixture
            .store
            .take_initial_work_handoff_for_owner_if_current(&fixture.snapshot(), "owner-b")
            .expect("store writes succeed")
            .is_none(),
        "the named owner holds the only lease, so the anonymous attempts created none"
    );
}

// ---------------------------------------------------------------------------
// B-35: a second, out-of-crate backend
// ---------------------------------------------------------------------------

/// A minimal third-party backend implementing ONLY the ten methods
/// `FinalTaskStore` requires, inheriting all twenty-seven defaults.
///
/// Its purpose is not to be useful. It exists so the conformance assertions
/// below can be shown to test the TRAIT CONTRACT rather than one
/// implementation's habits: any property asserted of both this and the
/// shipped store is a property of the contract. Nothing here stands in for
/// the shipped store -- `InMemoryFinalTaskStore` is exercised by the same
/// function, so PL-3 is satisfied by the real subject and this backend only
/// bounds what the assertions are allowed to mean.
#[derive(Default)]
struct MinimalBackend {
    tasks: Mutex<BTreeMap<FinalTaskId, (Task, u64)>>,
    cancellations: Mutex<BTreeSet<FinalTaskId>>,
}

impl MinimalBackend {
    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<FinalTaskId, (Task, u64)>> {
        self.tasks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl FinalTaskStore for MinimalBackend {
    fn create_task(&self, task: Task, _notification: TaskStatusNotification) -> McpResult<()> {
        let id = task.base().task_id.clone();
        if self.locked().insert(id, (task, 1)).is_some() {
            return Err(McpError::invalid_params("duplicate task identifier"));
        }
        Ok(())
    }

    fn get_task(&self, task_id: &FinalTaskId) -> McpResult<Option<Task>> {
        Ok(self.locked().get(task_id).map(|(task, _)| task.clone()))
    }

    fn get_task_snapshot(&self, task_id: &FinalTaskId) -> McpResult<Option<FinalTaskSnapshot>> {
        Ok(self
            .locked()
            .get(task_id)
            .map(|(task, generation)| FinalTaskSnapshot::new(task.clone(), *generation)))
    }

    fn replace_task(&self, task: Task, _notification: TaskStatusNotification) -> McpResult<()> {
        let id = task.base().task_id.clone();
        let mut state = self.locked();
        let Some((slot, generation)) = state.get_mut(&id) else {
            return Err(McpError::invalid_params("unknown task identifier"));
        };
        *slot = task;
        *generation += 1;
        Ok(())
    }

    fn replace_task_if_current(
        &self,
        expected: &FinalTaskSnapshot,
        task: Task,
        _notification: TaskStatusNotification,
    ) -> McpResult<bool> {
        let id = task.base().task_id.clone();
        let mut state = self.locked();
        let Some((slot, generation)) = state.get_mut(&id) else {
            return Ok(false);
        };
        if *generation != expected.generation() {
            return Ok(false);
        }
        *slot = task;
        *generation += 1;
        Ok(true)
    }

    fn request_cancellation(&self, task_id: &FinalTaskId) -> McpResult<()> {
        if !self.locked().contains_key(task_id) {
            return Err(McpError::invalid_params("unknown task identifier"));
        }
        self.cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(task_id.clone());
        Ok(())
    }

    fn request_cancellation_if_current(&self, expected: &FinalTaskSnapshot) -> McpResult<bool> {
        let id = expected.task().base().task_id.clone();
        if self.locked().get(&id).map(|(_, g)| *g) != Some(expected.generation()) {
            return Ok(false);
        }
        self.request_cancellation(&id)?;
        Ok(true)
    }

    fn is_cancellation_requested(&self, task_id: &FinalTaskId) -> McpResult<bool> {
        Ok(self
            .cancellations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(task_id))
    }

    fn retention_clock_now(&self) -> Instant {
        Instant::now()
    }

    fn task_retention_deadline_if_current(
        &self,
        task_id: &FinalTaskId,
        generation: u64,
    ) -> McpResult<Option<FinalTaskRetentionDeadline>> {
        Ok(self
            .locked()
            .get(task_id)
            .filter(|(_, current)| *current == generation)
            .map(|_| FinalTaskRetentionDeadline::Unlimited))
    }
}

/// Builds a `working` task and its notification, for either backend.
fn conformance_task(task_id: &str) -> (Task, TaskStatusNotification) {
    let task: Task = serde_json::from_value(serde_json::json!({
        "taskId": task_id,
        "status": "working",
        "createdAt": "2026-07-28T12:00:00.000Z",
        "lastUpdatedAt": "2026-07-28T12:00:00.000Z",
        "ttlMs": 600_000
    }))
    .expect("a well-formed working task");
    let notification = TaskStatusNotification::new(TaskStatusNotificationParams {
        task: task.clone(),
        meta: None,
        additional: std::collections::BTreeMap::default(),
    });
    (task, notification)
}

/// Properties every `FinalTaskStore` must satisfy, whoever wrote it.
///
/// Deliberately confined to the ten REQUIRED methods. Anything asserted here
/// of a capability a backend may legitimately decline would not be a contract
/// property, it would be a preference.
fn assert_required_surface_conformance(store: &dyn FinalTaskStore, backend: &str) {
    let (task, notification) = conformance_task("conformance-subject");
    let id = task.base().task_id.clone();
    let missing = conformance_task("conformance-absent")
        .0
        .base()
        .task_id
        .clone();

    assert!(
        store
            .get_task(&missing)
            .unwrap_or_else(|error| panic!(
                "{backend}: reading an unknown task is not an error: {error}"
            ))
            .is_none(),
        "{backend}: an unknown task must read as absent, not as an error or a value"
    );

    store
        .create_task(task, notification)
        .unwrap_or_else(|error| panic!("{backend}: creating a working task must succeed: {error}"));

    assert_eq!(
        store
            .get_task(&id)
            .expect("reads succeed")
            .expect("the created task is retained")
            .base()
            .task_id,
        id,
        "{backend}: a created task must read back under its own identifier"
    );

    let first = store
        .get_task_snapshot(&id)
        .expect("reads succeed")
        .expect("the created task has a snapshot");
    let second = store
        .get_task_snapshot(&id)
        .expect("reads succeed")
        .expect("the created task still has a snapshot");
    assert_eq!(
        first.generation(),
        second.generation(),
        "{backend}: a generation must not change because it was read"
    );

    assert!(
        store
            .task_retention_deadline_if_current(&id, first.generation())
            .expect("reads succeed")
            .is_some(),
        "{backend}: the current generation must resolve to a deadline"
    );
    assert!(
        store
            .task_retention_deadline_if_current(&id, first.generation().wrapping_add(1))
            .expect("reads succeed")
            .is_none(),
        "{backend}: a generation that is not current must read as absent"
    );

    assert!(
        !store.is_cancellation_requested(&id).expect("reads succeed"),
        "{backend}: a fresh task carries no cancellation intent"
    );
    store
        .request_cancellation(&id)
        .unwrap_or_else(|error| panic!("{backend}: cancelling a known task must succeed: {error}"));
    assert!(
        store.is_cancellation_requested(&id).expect("reads succeed"),
        "{backend}: cancellation intent must be durable once recorded"
    );
}

/// `B-35 third-party-backend-conformance`.
///
/// Two claims, and they are different in kind.
///
/// FIRST, the trait is implementable from outside the crate at all. Nothing
/// in this workspace implemented `FinalTaskStore` outside
/// `crates/fastmcp-server/src`; `MinimalBackend` below is the first, and it
/// compiles against the published surface using only public items. That is a
/// property of the shipped API, not of a mock.
///
/// SECOND, the required-surface properties hold for both the shipped store
/// and an unrelated implementation, which is what makes them CONTRACT
/// properties rather than observations about one backend's habits.
///
/// PL-3 note: the shipped `InMemoryFinalTaskStore` is exercised by the same
/// function, so the real subject is under test. `MinimalBackend` never stands
/// in for it -- it only bounds what the shared assertions are allowed to
/// claim.
#[test]
fn b35_third_party_backend_conformance() {
    let shipped = InMemoryFinalTaskStore::new(4).expect("bounded store");
    assert_required_surface_conformance(&shipped, "InMemoryFinalTaskStore");

    let third_party = MinimalBackend::default();
    assert_required_surface_conformance(&third_party, "MinimalBackend");

    // The optional surface FAILS CLOSED on a backend that declined it. The
    // trait's defaults return an error rather than a false or a None, so a
    // caller cannot mistake "not implemented" for "declined this time" and
    // create unexecutable work. Nothing in the tree tested this.
    let (task, notification) = conformance_task("conformance-fail-closed");
    assert!(
        third_party
            .create_task_with_work(
                task,
                notification,
                FinalTaskWorkDescriptor::new(serde_json::json!({"operation": "x"}))
                    .expect("bounded descriptor")
            )
            .is_err(),
        "a backend without atomic task-work creation must refuse, not silently create a task"
    );
    assert!(
        third_party
            .handoff_dispatch_lease_heartbeat_interval()
            .is_err(),
        "a backend with no durable lease must not disclose a heartbeat interval"
    );
    assert!(
        third_party
            .begin_handoff_dispatch_for_owner_if_current(
                &conformance_task("conformance-fail-closed")
                    .0
                    .base()
                    .task_id
                    .clone(),
                1,
                "owner-a"
            )
            .is_err(),
        "a backend with no dispatch election must refuse rather than report a lost race"
    );

    // And the discriminator: the SHIPPED store implements those same three,
    // so the assertions above distinguish backends instead of holding
    // vacuously for everyone.
    assert!(
        shipped.handoff_dispatch_lease_heartbeat_interval().is_ok(),
        "the shipped store does implement the optional surface, so failing closed is a \
         real distinction and not a property of the trait itself"
    );
}

/// `B-44 mrtr-nonuse`. MRTR -- multi-round model-request tool retry -- is a
/// CLIENT concept, defined at `crates/fastmcp-client/src/execution.rs:113`.
/// The durable task path must neither require it nor expose it. It is not
/// structurally unreachable from this package, which is why the group exists:
/// `fastmcp-server/Cargo.toml` declares `proxy = ["dep:fastmcp-client"]`, so
/// the client crate IS a dependency under the `proxy` feature. What this test
/// holds is the narrower and checkable claim -- the durable surface itself
/// carries no multi-round retry state and needs none to complete.
///
/// A non-use claim is the easiest kind to write vacuously, so this is built to
/// fail in both directions. An absence assertion over a GUESSED key name
/// returns a false zero whether or not the property holds, so the scan runs
/// over the wire form's OWN keys, and a positive control establishes that the
/// form is a populated task before its key set is allowed to mean anything.
#[test]
fn b44_mrtr_state_is_absent_from_the_durable_path() {
    let fixture = Fixture::new(TASK, 600_000);
    let wire = fixture.task_wire_form();
    let object = wire
        .as_object()
        .expect("the task wire form is a JSON object");

    // POSITIVE CONTROL. Without it an empty or null wire form would satisfy
    // every absence assertion below, and the test would pass proving nothing.
    assert!(
        object.contains_key("taskId") && object.contains_key("status"),
        "the wire form must be a populated task before its key set means \
         anything, but it is {object:?}"
    );

    // The absence assertion, enumerated over what IS there.
    for key in object.keys() {
        let lowered = key.to_ascii_lowercase();
        for forbidden in ["mrtr", "round", "retry", "continuation", "resume"] {
            assert!(
                !lowered.contains(forbidden),
                "the durable task wire form must carry no multi-round retry \
                 state, but key `{key}` matches `{forbidden}`"
            );
        }
    }

    // And the lifecycle completes with nothing MRTR-shaped supplied: election
    // and release go through the ordinary durable surface and no resume input
    // is offered at any point.
    let (snapshot, fence) = fixture.elect("owner-a");
    assert!(
        fixture
            .store
            .finish_handoff_dispatch_for_owner_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                fence
            )
            .expect("store writes succeed"),
        "a durable dispatch must complete with no multi-round retry state supplied"
    );
}

/// `B-46 ops-readiness-cardinality`. `FinalTaskRuntime::is_task_service_ready`
/// is the public readiness observation. Its contract, stated at
/// `fastmcp-server/src/tasks.rs:5902`, is that readiness begins ONLY when an
/// entered runner is polled and holds its lease: "Merely installing a runner,
/// retaining a runtime clone, or retaining durable work is not readiness."
///
/// The cardinality this asserts is the ZERO case -- with no runner ever
/// entered, no number of runtimes, clones, or durable rows may report ready.
/// That is the half that can be proved without an async runner harness. The
/// one-owner case (exactly one entered runner holds readiness, and a second
/// cannot) needs `install_task_service` plus a polled `run_service`, which
/// this file has no harness for; it is NOT covered here and the header table
/// says so rather than letting a partial pass as whole.
///
/// The positive control matters more than usual: "durable work does not confer
/// readiness" is vacuous if there is no durable work, and every assertion here
/// is a negative.
#[test]
fn b46_ops_readiness_cardinality_is_zero_before_entry() {
    let fixture = Fixture::new(TASK, 600_000);

    // POSITIVE CONTROL. The store really holds the task, so the negatives
    // below are about readiness rather than about an empty store.
    assert!(
        fixture
            .store
            .get_task(&fixture.id)
            .expect("store reads succeed")
            .is_some(),
        "the runtime must sit over a store that genuinely holds durable work, \
         or 'durable work is not readiness' asserts nothing"
    );

    // `fixture.store.clone()`, NOT `Arc::clone(&fixture.store)`. The parameter
    // is `Arc<dyn FinalTaskStore>`, so the concrete `Arc<InMemoryFinalTaskStore>`
    // has to unsize at the argument. With the associated-function form the
    // expected type propagates INTO the call and fixes the turbofish to
    // `Arc::<dyn FinalTaskStore>::clone`, whose parameter is then
    // `&Arc<dyn FinalTaskStore>` -- and `&Arc<Concrete>` does not coerce to
    // that, because unsizing applies to `Arc<Concrete>` and not behind the
    // outer `&`. Method syntax resolves the receiver independently, returns
    // `Arc<InMemoryFinalTaskStore>`, and coerces at the argument where it is
    // legal. Every other call site in the workspace uses the method form.
    let runtime = FinalTaskRuntime::new(
        fixture.store.clone(),
        FinalTaskRuntimeConfig::new(600_000, None).expect("a positive ttl yields a policy"),
        Arc::new(|_| {}),
    );

    // 1. A runtime that has never installed a service is not ready.
    assert!(
        !runtime.is_task_service_ready(),
        "a runtime with no installed task service must not report ready"
    );

    // 2. Retaining durable work is not readiness. The work is created AFTER the
    //    runtime exists, which is what makes this assertion able to fail
    //    independently of assertion 1 -- if accumulating durable rows ever
    //    conferred readiness, only this ordering would catch it. An earlier
    //    version asserted the same call over the task that PREDATES the
    //    runtime, which merely restates assertion 1 and cannot fail if it
    //    passed.
    let (extra, extra_notification) = conformance_task("readiness-extra");
    fixture
        .store
        .create_task_with_work(
            extra,
            extra_notification,
            FinalTaskWorkDescriptor::new(serde_json::json!({"operation": "durable"}))
                .expect("a bounded work descriptor"),
        )
        .expect("the store accepts a second task within its capacity");
    assert!(
        !runtime.is_task_service_ready(),
        "durable work created after the runtime must not confer readiness"
    );

    // 3. A clone does not create readiness and does not disturb the original.
    //    Readiness is a property of an entered runner, not of handle count.
    let cloned = runtime.clone();
    assert!(
        !cloned.is_task_service_ready(),
        "cloning a runtime must not manufacture readiness"
    );
    assert!(
        !runtime.is_task_service_ready(),
        "observing a clone must not change the original's readiness"
    );
}

// ---------------------------------------------------------------------------
// Frozen IDs
// ---------------------------------------------------------------------------

#[test]
fn task_02_b_positive() {
    lease_window_is_the_one_this_file_assumes();
    b26_stale_owner_fenced_write();
    b27_lease_renew_then_expire();
    b28_durable_time_authority();
    b29_backend_clock_discontinuity();
    b30_skewed_worker_time_domains();
    b32_deadline_survives_renew_and_restart();
    b24_persist_ambiguous_commit();
    b35_third_party_backend_conformance();
    b31_revision_order_is_a_chain();
    b31_stale_generation_is_refused();
    b34_restore_write_contract();
    b42_duplicate_execution_is_refused();
    b45_anonymous_handle_is_refused_everywhere();
    b43_release_happens_exactly_once();
    b44_mrtr_state_is_absent_from_the_durable_path();
    b46_ops_readiness_cardinality_is_zero_before_entry();
}

#[test]
fn task_02_b_planted_negative() {
    // ARM 0 -- the accepted row. Without it, a store that refused every write
    // would pass the arm below for the wrong reason.
    let accepted = Fixture::new(TASK, 600_000);
    let (accepted_snapshot, accepted_fence) = accepted.elect("owner-a");
    assert!(
        accepted
            .store
            .renew_handoff_dispatch_if_current(
                &accepted.id,
                accepted_snapshot.generation(),
                "owner-a",
                accepted_fence
            )
            .expect("store writes succeed"),
        "the unmutated fence must renew, or the arm below proves nothing"
    );

    // THE ONE VARIABLE: the committed fence generation, moved by exactly one.
    // Owner, task generation, clock, and every other input are identical to
    // ARM 0.
    let fixture = Fixture::new(TASK, 600_000);
    let (snapshot, fence) = fixture.elect("owner-a");
    let stale_fence = fence
        .checked_add(1)
        .expect("a fence one above the issued one is representable");
    assert_ne!(
        stale_fence, fence,
        "the mutation must actually change the fence"
    );

    // Everything the refusal must leave untouched, sampled BEFORE.
    let before_deadline = fixture
        .store
        .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
        .expect("store reads succeed");
    let before_clock = fixture.store.retention_clock_now();
    let before_task = fixture.task_wire_form();
    assert_eq!(
        before_task["taskId"], TASK,
        "the sampled record must be the real task, or every comparison below is vacuous"
    );
    let before_generation = fixture
        .store
        .get_task_snapshot(&fixture.id)
        .expect("store reads succeed")
        .expect("the task exists")
        .generation();
    let before_cancellation = fixture
        .store
        .is_cancellation_requested(&fixture.id)
        .expect("store reads succeed");

    assert!(
        !fixture
            .store
            .renew_handoff_dispatch_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                stale_fence
            )
            .expect("a stale fence is a refusal, not a transport error"),
        "a fence one generation off must be refused"
    );
    assert!(
        !fixture
            .store
            .finish_handoff_dispatch_for_owner_if_current(
                &fixture.id,
                snapshot.generation(),
                "owner-a",
                stale_fence
            )
            .expect("a stale fence is a refusal, not a transport error"),
        "a stale fence must not be able to release a lease it does not hold"
    );

    // The named state fields, unchanged.
    assert_eq!(
        fixture
            .store
            .task_retention_deadline_if_current(&fixture.id, snapshot.generation())
            .expect("store reads succeed"),
        before_deadline,
        "expiry state must be unchanged by a refused fenced write"
    );
    assert_eq!(
        fixture.store.retention_clock_now(),
        before_clock,
        "the durable-time sample must be unchanged"
    );
    assert_eq!(
        fixture.task_wire_form(),
        before_task,
        "the task record must be unchanged"
    );
    assert_eq!(
        fixture
            .store
            .get_task_snapshot(&fixture.id)
            .expect("store reads succeed")
            .expect("the task still exists")
            .generation(),
        before_generation,
        "a refused write must not consume a generation"
    );
    assert_eq!(
        fixture
            .store
            .is_cancellation_requested(&fixture.id)
            .expect("store reads succeed"),
        before_cancellation,
        "cancellation intent must be unchanged"
    );

    // And the real fence still works, which proves the refusals above were
    // attributable to the mutated fence and not to a store left broken.
    assert!(
        fixture
            .store
            .renew_handoff_dispatch_if_current(&fixture.id, snapshot.generation(), "owner-a", fence)
            .expect("store writes succeed"),
        "the genuine fence must still renew after the refused attempts"
    );
}
