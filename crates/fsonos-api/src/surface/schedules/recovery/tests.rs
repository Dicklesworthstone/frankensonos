//! Real saved files and simulator SOAP; faults affect replies, not fake state.

use super::*;
use crate::surface::Survey;
use crate::surface::schedules::{SLEEP_FILE, SleepRequest};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control::{configure_sleep_timer, get_remaining_sleep_timer};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};

static NEXT_DIR: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let root = std::env::temp_dir();
        for _ in 0..128 {
            let path = root.join(format!("fsonos-sleep-recovery-{}-{}", std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)));
            match std::fs::create_dir(&path) {
                Ok(()) => return Self(path),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => panic!("test directory: {error}"),
            }
        }
        panic!("no test directory slot");
    }

    fn path(&self) -> PathBuf {
        self.0.join(SLEEP_FILE)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Replies {
    fail: Mutex<HashSet<IpAddr>>,
    count: AtomicUsize,
    hold: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

struct ReplyTransport {
    inner: SimLan,
    replies: Arc<Replies>,
}

impl Transport for ReplyTransport {
    fn soap_post(&self, host: IpAddr, path: &str, header: &str, body: &str) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, header, body)?;
        if header.contains("#GetRemainingSleepTimerDuration") {
            self.replies.count.fetch_add(1, Ordering::SeqCst);
            if self.replies.fail.lock().unwrap().contains(&host) {
                return Err(ProtoError::Network {
                    target: "synthetic-sleep-player".into(),
                    detail: "lost the sleep-timer read reply".into(),
                });
            }
            let hold = self.replies.hold.lock().unwrap().take();
            if let Some((arrived, resume)) = hold {
                let _ = arrived.send(());
                let _ = resume.recv_timeout(Duration::from_secs(5));
            }
        }
        Ok(reply)
    }
}

struct House {
    sim: SimHandle,
    rooms: Arc<Mutex<Vec<HouseholdState>>>,
    replies: Arc<Replies>,
    scratch: Scratch,
}

impl House {
    fn new(s2: bool) -> Self {
        let model = || if s2 { SimModel::One } else { SimModel::Play5Gen1 };
        let specs = [SimPlayerSpec::new("RecoverA", model()), SimPlayerSpec::new("RecoverB", model())];
        let builder = SimHousehold::builder();
        let sim = if s2 { builder.s2(specs) } else { builder.s1(specs) }.spawn().unwrap();
        let mut household = HouseholdState::default();
        let ip = sim.player("RecoverA").unwrap().ip;
        household.apply_topology(&get_zone_group_state(&sim.lan(), ip).unwrap());
        Self {
            sim,
            rooms: Arc::new(Mutex::new(vec![household])),
            replies: Arc::new(Replies::default()),
            scratch: Scratch::new(),
        }
    }

    fn id(&self, room: &str) -> PlayerId {
        PlayerId(self.sim.player(room).unwrap().uuid.clone())
    }

    fn ip(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn surface(&self) -> Arc<Surface> {
        let rooms = Arc::clone(&self.rooms);
        let survey: Survey = Box::new(move |_| Ok(rooms.lock().unwrap().clone()));
        Arc::new(Surface::new(
            Box::new(ReplyTransport { inner: self.sim.lan(), replies: Arc::clone(&self.replies) }),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        ).with_sleep(Sleep::default().saved_in(self.scratch.path())))
    }

    fn saved(&self) -> Vec<SleepTimer> {
        serde_json::from_slice::<SavedTimers>(&std::fs::read(self.scratch.path()).unwrap())
            .unwrap().timers
    }

    fn seed(&self) -> SleepTimer {
        let surface = self.surface();
        set(&surface, "RecoverA", "45m", false);
        self.saved().remove(0)
    }

    fn hold_read(&self) -> (mpsc::Receiver<()>, mpsc::Sender<()>) {
        let (arrived, entered) = mpsc::sync_channel(1);
        let (resume, waiting) = mpsc::channel();
        *self.replies.hold.lock().unwrap() = Some((arrived, waiting));
        (entered, resume)
    }
}

fn set(surface: &Surface, room: &str, duration: &str, extend: bool) {
    surface.set_sleep_timer(&Client::Cli, &SleepRequest {
        zone: room.into(), duration: Some(duration.into()), extend, cancel: false,
    }).unwrap();
}

fn cancel_timer(surface: &Surface, room: &str) {
    surface.set_sleep_timer(&Client::Cli, &SleepRequest {
        zone: room.into(), duration: None, extend: false, cancel: true,
    }).unwrap();
}

fn sleep(surface: &Surface) -> &Arc<Sleep> {
    surface.sleep.as_ref().unwrap()
}

#[test]
fn failed_startup_reads_survive_another_restart_on_both_generations() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let original = h.seed();
        h.replies.fail.lock().unwrap().insert(h.ip("RecoverA"));
        let unavailable = h.surface();
        assert_eq!(unavailable.adopt_sleep_timers(), 0);
        assert_eq!(h.saved(), [original.clone()]);
        assert!(sleep(&unavailable).timers.all().is_empty());
        drop(unavailable);
        h.replies.fail.lock().unwrap().clear();
        let recovered = h.surface();
        assert_eq!(recovered.adopt_sleep_timers(), 1);
        assert_eq!(sleep(&recovered).timers.all(), [original]);
        assert_eq!(recovered.adopt_sleep_timers(), 0);
    }
}

#[test]
fn unrelated_timer_writes_preserve_the_unreachable_startup_candidate() {
    let h = House::new(true);
    let original = h.seed();
    h.replies.fail.lock().unwrap().insert(h.ip("RecoverA"));
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 0);
    set(&surface, "RecoverB", "15m", false);
    let saved = h.saved();
    assert_eq!(saved.len(), 2);
    assert!(saved.contains(&original));
    cancel_timer(&surface, "RecoverB");
    assert_eq!(h.saved(), [original]);
}

#[test]
fn first_new_command_cannot_overwrite_an_unread_startup_snapshot() {
    let h = House::new(true);
    let original = h.seed();
    let surface = h.surface();
    // No explicit adoption has happened in this daemon yet.
    set(&surface, "RecoverB", "10m", false);
    assert!(h.saved().contains(&original));
    assert_eq!(h.saved().len(), 2);
    assert_eq!(surface.adopt_sleep_timers(), 1);
}

#[test]
fn discovery_recovery_rearms_on_a_scheduler_worker_without_rewriting_the_speaker() {
    let h = House::new(false);
    let original = h.seed();
    let rooms = std::mem::take(&mut *h.rooms.lock().unwrap());
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 0);
    assert_eq!(h.saved(), [original.clone()]);
    *h.rooms.lock().unwrap() = rooms;
    sleep(&surface).writing.lock().unwrap().next = Instant::now();
    let before = h.sim.soap_log().len();
    let scheduler = Scheduler::new(&surface);
    scheduler.tick();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(sleep(&surface).timers.all(), [original]);
    assert_eq!(h.sim.soap_log().len() - before, 1, "only the backstop read");
    let reads = h.replies.count.load(Ordering::SeqCst);
    for _ in 0..100 { scheduler.tick(); }
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(h.replies.count.load(Ordering::SeqCst), reads, "healthy timers do not poll");
}

#[test]
fn an_accepted_cancel_fences_a_delayed_recovery_read() {
    let h = House::new(true);
    h.seed();
    let surface = h.surface();
    let (entered, resume) = h.hold_read();
    let reading = Arc::clone(&surface);
    let worker = std::thread::spawn(move || reading.adopt_sleep_timers());
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    cancel_timer(&surface, "RecoverA");
    resume.send(()).unwrap();
    assert_eq!(worker.join().unwrap(), 0);
    assert!(sleep(&surface).timers.all().is_empty());
    assert!(h.saved().is_empty());
    assert_eq!(get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA")).unwrap(), None);
}

#[test]
fn a_replacement_timer_wins_over_a_delayed_recovery_read() {
    let h = House::new(true);
    let original = h.seed();
    let surface = h.surface();
    let (entered, resume) = h.hold_read();
    let reading = Arc::clone(&surface);
    let worker = std::thread::spawn(move || reading.adopt_sleep_timers());
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    set(&surface, "RecoverA", "15m", false);
    let replacement = h.saved();
    assert_eq!(replacement.len(), 1);
    assert!(replacement[0].ends < original.ends);
    resume.send(()).unwrap();
    assert_eq!(worker.join().unwrap(), 0);
    assert_eq!(sleep(&surface).timers.all(), replacement);
    assert_eq!(h.saved(), replacement);
}

#[test]
fn extending_an_unadopted_backstop_does_not_add_its_five_minute_allowance() {
    let h = House::new(true);
    let original = h.seed();
    let surface = h.surface();
    set(&surface, "RecoverA", "5m", true);
    let timers = sleep(&surface).timers.all();
    assert_eq!(timers.len(), 1);
    assert_eq!(timers[0].ends, original.ends + chrono::TimeDelta::minutes(5));
    assert_eq!(surface.adopt_sleep_timers(), 0);
}

#[test]
fn an_invalid_extension_does_not_consume_the_pending_timer() {
    let h = House::new(false);
    h.seed();
    let surface = h.surface();
    assert!(surface.set_sleep_timer(&Client::Cli, &SleepRequest {
        zone: "RecoverA".into(), duration: Some("23h".into()), extend: true, cancel: false,
    }).is_err());
    assert_eq!(surface.adopt_sleep_timers(), 1);
}

#[test]
fn speaker_cancellation_or_replacement_is_observed_but_never_overwritten() {
    for replacement in [None, Some(90 * 60)] {
        let h = House::new(true);
        h.seed();
        configure_sleep_timer(&h.sim.lan(), h.ip("RecoverA"), replacement).unwrap();
        let surface = h.surface();
        let before = h.sim.soap_log().len();
        assert_eq!(surface.adopt_sleep_timers(), 0);
        assert_eq!(h.sim.soap_log().len() - before, 1, "verification is read-only");
        assert!(h.saved().is_empty());
        assert!(sleep(&surface).timers.all().is_empty());
        let left = get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA")).unwrap();
        assert_eq!(left.is_some(), replacement.is_some());
    }
}

#[test]
fn expired_or_overflowing_deadlines_are_retired_without_discovery_or_speaker_io() {
    let h = House::new(true);
    let mut timer = h.seed();
    for ends in [Utc::now() - chrono::TimeDelta::hours(1), DateTime::<Utc>::MAX_UTC] {
        timer.ends = ends;
        file::replace(&h.scratch.path(), &serde_json::to_vec(&SavedTimers {
            timers: vec![timer.clone()],
        }).unwrap()).unwrap();
        h.rooms.lock().unwrap().clear();
        let surface = h.surface();
        let before = h.replies.count.load(Ordering::SeqCst);
        assert_eq!(surface.adopt_sleep_timers(), 0);
        assert!(h.saved().is_empty());
        assert_eq!(h.replies.count.load(Ordering::SeqCst), before);
    }
}

#[test]
fn corrupt_snapshot_is_preserved_and_later_loading_cannot_resurrect_a_local_cancel() {
    let h = House::new(true);
    let original = h.seed();
    std::fs::write(h.scratch.path(), b"{ broken snapshot").unwrap();
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 0);
    cancel_timer(&surface, "RecoverA");
    assert_eq!(std::fs::read(h.scratch.path()).unwrap(), b"{ broken snapshot");
    assert!(!sleep(&surface).writing.lock().unwrap().loaded);
    // Even if the original file is restored after that explicit cancellation,
    // its old timer may no longer grant automatic control in this process.
    file::replace(&h.scratch.path(), &serde_json::to_vec(&SavedTimers {
        timers: vec![original],
    }).unwrap()).unwrap();
    assert_eq!(surface.adopt_sleep_timers(), 0);
    assert!(h.saved().is_empty());
}

#[test]
fn failed_publication_is_retried_without_replaying_the_timer_command() {
    let h = House::new(true);
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 0); // Initialize the missing file.
    std::fs::create_dir(h.scratch.path()).unwrap(); // Rename-to-file must fail.
    set(&surface, "RecoverA", "45m", false);
    assert!(sleep(&surface).writing.lock().unwrap().dirty);
    let writes = h.sim.soap_log().len();
    std::fs::remove_dir(h.scratch.path()).unwrap();
    sleep(&surface).writing.lock().unwrap().next = Instant::now();
    let scheduler = Scheduler::new(&surface);
    scheduler.tick();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert!(!sleep(&surface).writing.lock().unwrap().dirty);
    assert_eq!(h.saved().len(), 1);
    assert_eq!(h.sim.soap_log().len(), writes, "only local persistence is retried");
}

#[test]
fn recovery_is_single_flight_and_does_not_block_the_tick_thread() {
    let h = House::new(false);
    h.seed();
    let surface = h.surface();
    let (entered, resume) = h.hold_read();
    let scheduler = Arc::new(Scheduler::new(&surface));
    scheduler.tick();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let count = h.replies.count.load(Ordering::SeqCst);
    let other = Arc::clone(&scheduler);
    let (done, receive) = mpsc::channel();
    let ticker = std::thread::spawn(move || {
        for _ in 0..100 { other.tick(); }
        done.send(()).unwrap();
    });
    receive.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(h.replies.count.load(Ordering::SeqCst), count);
    assert!(reserve(sleep(&surface), true).is_none());
    resume.send(()).unwrap();
    ticker.join().unwrap();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert_eq!(sleep(&surface).timers.all().len(), 1);
}

#[test]
fn a_former_coordinator_is_not_adopted_into_its_new_group() {
    let h = House::new(true);
    let original = h.seed();
    let id = h.id("RecoverA");
    h.rooms.lock().unwrap()[0].groups.retain(|group| group.coordinator != id);
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 0);
    assert_eq!(h.saved(), [original]);
    assert_eq!(h.replies.count.load(Ordering::SeqCst), 0);
}

#[test]
fn a_failed_worker_spawn_releases_admission_and_preserves_backoff() {
    let h = House::new(true);
    h.seed();
    let surface = h.surface();
    let sleep = sleep(&surface);
    let mut expected = FIRST;
    for _ in 0..10 {
        let permit = reserve(sleep, true).unwrap();
        assert!(reserve(sleep, true).is_none());
        let before = Instant::now();
        // A closure dropped by thread::Builder::spawn on failure owns this
        // exact permit; dropping it must leave recovery eligible later.
        drop(permit);
        let state = sleep.writing.lock().unwrap();
        assert!(!state.busy);
        assert!(state.next >= before + expected);
        assert!(state.delay <= MAX);
        drop(state);
        assert!(reserve(sleep, false).is_none());
        expected = expected.saturating_mul(2).min(MAX);
    }
}

#[test]
fn malformed_duplicate_or_oversized_files_never_partially_initialize() {
    let h = House::new(true);
    let timer = h.seed();
    let duplicate = serde_json::to_vec(&SavedTimers { timers: vec![timer.clone(), timer] }).unwrap();
    for bytes in [b"{".to_vec(), duplicate, vec![b' '; usize::try_from(MAX_FILE + 1).unwrap()]] {
        std::fs::write(h.scratch.path(), &bytes).unwrap();
        let mut state = State::default();
        assert!(state.load(&h.scratch.path()).is_err());
        assert!(!state.loaded);
        assert!(state.pending.is_empty());
        assert_eq!(std::fs::read(h.scratch.path()).unwrap(), bytes);
    }
}

#[test]
fn atomic_publication_preserves_unowned_staging_and_uses_private_permissions() {
    let dir = Scratch::new();
    let legacy = dir.path().with_extension("json.partial");
    std::fs::write(&legacy, b"another writer").unwrap();
    file::replace(&dir.path(), b"first").unwrap();
    file::replace(&dir.path(), b"second").unwrap();
    assert_eq!(std::fs::read(dir.path()).unwrap(), b"second");
    assert_eq!(std::fs::read(&legacy).unwrap(), b"another writer");
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 2);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777, 0o600);
    }
}

#[test]
fn parallel_atomic_publications_never_mix_snapshots() {
    let dir = Scratch::new();
    let path = dir.path();
    let payloads = [vec![b'a'; 8192], vec![b'b'; 8192]];
    std::thread::scope(|scope| {
        for bytes in &payloads {
            let path = &path;
            scope.spawn(move || {
                for _ in 0..20 { file::replace(path, bytes).unwrap(); }
            });
        }
    });
    let bytes = std::fs::read(&path).unwrap();
    assert!(payloads.contains(&bytes));
    assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
}

#[test]
fn shutdown_prevents_a_late_read_from_adopting_a_new_timer() {
    let h = House::new(true);
    let original = h.seed();
    let surface = h.surface();
    let (entered, resume) = h.hold_read();
    let scheduler = Scheduler::new(&surface);
    scheduler.tick();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    scheduler.shut_down(Duration::ZERO);
    resume.send(()).unwrap();
    assert!(scheduler.wait(Duration::from_secs(5)));
    assert!(sleep(&surface).timers.all().is_empty());
    assert_eq!(h.saved(), [original]);
}

#[test]
fn an_overdue_verified_timer_is_due_immediately_not_restarted_for_two_minutes() {
    let h = House::new(true);
    let mut original = h.seed();
    original.ends = Utc::now() - chrono::TimeDelta::seconds(1);
    file::replace(&h.scratch.path(), &serde_json::to_vec(&SavedTimers {
        timers: vec![original.clone()],
    }).unwrap()).unwrap();
    configure_sleep_timer(&h.sim.lan(), h.ip("RecoverA"), Some(299)).unwrap();
    let surface = h.surface();
    assert_eq!(surface.adopt_sleep_timers(), 1);
    let due = sleep(&surface).timers.due(Utc::now());
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].ends, original.ends);
    assert!(due[0].fading);
}

#[test]
fn partial_startup_adoption_preserves_both_verified_and_unresolved_timers() {
    let h = House::new(true);
    {
        let first = h.surface();
        set(&first, "RecoverA", "45m", false);
        set(&first, "RecoverB", "30m", false);
    }
    let original = h.saved();
    h.replies.fail.lock().unwrap().insert(h.ip("RecoverA"));
    let restarted = h.surface();
    assert_eq!(restarted.adopt_sleep_timers(), 1);
    assert_eq!(h.saved(), original);
    assert_eq!(sleep(&restarted).timers.all().len(), 1);
    h.replies.fail.lock().unwrap().clear();
    assert_eq!(restarted.adopt_sleep_timers(), 1);
    assert_eq!(sleep(&restarted).timers.all().len(), 2);
    assert_eq!(h.saved(), original);
}
