use super::*;
use crate::live::{Live, LiveConfig, Snapshot, pump};
use crate::playback::Playback;
use crate::reconcile::Reconciler;
use fsonos_proto::control::{set_av_transport_uri, set_volume};
use fsonos_proto::net::Lan;
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::SystemTime;

const WAIT: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, PartialEq, Eq)]
enum Stage {
    Discovery,
    Description,
    Topology,
}

#[derive(Default)]
struct Gate {
    armed: AtomicBool,
    entered: AtomicBool,
    released: AtomicBool,
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::Release);
    }
}

struct Held {
    inner: Arc<dyn Transport + Send + Sync>,
    gate: Arc<Gate>,
    stage: Stage,
    calls: AtomicUsize,
    discoveries: AtomicUsize,
}

impl Held {
    fn hold(&self, stage: Stage) {
        if stage == self.stage
            && self.gate.armed.load(Ordering::Acquire)
            && !self.gate.entered.swap(true, Ordering::AcqRel)
        {
            let deadline = Instant::now() + WAIT;
            while !self.gate.released.load(Ordering::Acquire) {
                assert!(Instant::now() < deadline, "held inventory read timed out");
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}

impl Transport for Held {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let topology = action.trim_matches('"').ends_with("#GetZoneGroupState");
        if topology {
            self.calls.fetch_add(1, Ordering::Relaxed);
        }
        let result = self.inner.soap_post(host, path, action, body)?;
        if topology {
            self.hold(Stage::Topology);
        }
        Ok(result)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        let result = self.inner.http_get(url)?;
        self.hold(Stage::Description);
        Ok(result)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.discoveries.fetch_add(1, Ordering::Relaxed);
        let result = self.inner.discover(mx_secs, wait);
        self.hold(Stage::Discovery);
        result
    }
}

fn sim(generation: u8) -> SimHandle {
    let hardware = if generation == 1 {
        SimModel::Play5Gen1
    } else {
        SimModel::One
    };
    let rooms = ["A", "B"].map(|name| SimPlayerSpec::new(name, hardware));
    let builder = SimHousehold::builder();
    if generation == 1 {
        builder.s1(rooms)
    } else {
        builder.s2(rooms)
    }
    .spawn()
    .unwrap()
}

fn lan(sim: &SimHandle) -> Arc<Lan> {
    Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    )
}

fn transport(sim: &SimHandle, stage: Stage) -> Arc<Held> {
    Arc::new(Held {
        inner: Arc::new(sim.lan()),
        gate: Arc::new(Gate::default()),
        stage,
        calls: AtomicUsize::new(0),
        discoveries: AtomicUsize::new(0),
    })
}

struct Fixture {
    _release: Release,
    sim: SimHandle,
    transport: Arc<Held>,
    engine: Engine,
    model: Model,
}

impl Fixture {
    fn new(generation: u8, stage: Stage, renewal_due: bool) -> Self {
        let sim = sim(generation);
        let transport = transport(&sim, stage);
        let now = Instant::now();
        let at = if renewal_due {
            now.checked_sub(Duration::from_secs(1000)).unwrap()
        } else {
            now
        };
        let engine = Engine {
            transport: transport.clone(),
            lan: lan(&sim),
            config: LiveConfig::new(sim.players().iter().map(|p| p.ip).collect()),
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            wake: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let mut model = Model {
            ticked: (now, SystemTime::now()),
            remembered: Vec::new(),
            rec: Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), at),
            playback: Playback::default(),
            progress: progress::Watchdog::default(),
            sink: None,
            surveyed_at: None,
            last_error: None,
            route_checked: now,
        };
        let events = engine.survey(&mut model, at);
        engine.publish(&model, events);
        assert_eq!(model.rec.households.len(), 1);
        // Drain every initial callback before dispatch, without renewing yet.
        let count = wanted(&model.rec.households).len();
        assert_eq!(model.rec.subscriptions.len(), count);
        for _ in 0..count {
            let events = engine.next_event(&mut model, WAIT).unwrap();
            engine.publish(&model, events);
        }
        Self {
            _release: Release(Arc::clone(&transport.gate)),
            sim,
            transport,
            engine,
            model,
        }
    }

    fn start(&mut self, surveys: &mut Surveys) {
        self.transport.gate.armed.store(true, Ordering::Release);
        let at = Instant::now();
        surveys.start(&self.engine, &mut self.model);
        assert!(at.elapsed() < Duration::from_secs(1));
        wait_for(|| self.transport.gate.entered.load(Ordering::Acquire));
    }

    fn release(&self) {
        self.transport.gate.released.store(true, Ordering::Release);
    }

    fn finish(mut self, mut surveys: Surveys) {
        self.engine.stop.store(true, Ordering::Release);
        self.release();
        wait_for(|| {
            surveys.complete(&self.engine, &mut self.model);
            !surveys.busy()
        });
        self.engine.release(&mut self.model);
        self.sim.shutdown();
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let until = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < until, "inventory test did not converge");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn stalled_discovery_and_descriptions_leave_callbacks_and_renewals_responsive() {
    for generation in [1, 2] {
        for stage in [Stage::Discovery, Stage::Description] {
            let mut f = Fixture::new(generation, stage, true);
            let before = f.model.surveyed_at;
            let mut surveys = Surveys::default();
            f.start(&mut surveys);
            let player = f.sim.player("B").unwrap();
            let id = PlayerId(player.uuid.clone());
            set_volume(&f.sim.lan(), player.ip, 59).unwrap();
            wait_for(|| {
                pump::turn(&f.engine, &mut f.model);
                surveys.complete(&f.engine, &mut f.model);
                f.model
                    .playback
                    .of(&id)
                    .is_some_and(|p| p.volume == Some(59))
            });
            assert!(surveys.busy());
            assert_eq!(f.model.surveyed_at, before);
            assert!(!f.transport.gate.released.load(Ordering::Acquire));
            assert!(
                f.sim
                    .gena_log()
                    .iter()
                    .any(|e| matches!(e.event, GenaEvent::Renewed { .. }))
            );
            f.release();
            wait_for(|| {
                surveys.complete(&f.engine, &mut f.model);
                !surveys.busy()
            });
            assert!(f.model.surveyed_at > before);
            assert_eq!(f.model.playback.of(&id).unwrap().volume, Some(59));
            assert!(f.model.last_error.is_none());
            let calls = f.transport.calls.load(Ordering::Relaxed);
            for _ in 0..20 {
                surveys.complete(&f.engine, &mut f.model);
            }
            assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
            f.finish(surveys);
        }
    }
}

#[test]
fn refresh_requests_coalesce_without_overlapping_or_disappearing_on_completion() {
    let mut f = Fixture::new(2, Stage::Discovery, false);
    let before = f.transport.discoveries.load(Ordering::Relaxed);
    let mut surveys = Surveys::default();
    f.start(&mut surveys);
    for _ in 0..100 {
        surveys.refresh_soon();
        surveys.start(&f.engine, &mut f.model);
        surveys.complete(&f.engine, &mut f.model);
    }
    assert_eq!(f.transport.discoveries.load(Ordering::Relaxed), before + 1);
    f.release();
    wait_for(|| {
        surveys.complete(&f.engine, &mut f.model);
        !surveys.busy()
    });
    assert!(f.model.rec.schedule.due(Instant::now()));
    surveys.start(&f.engine, &mut f.model);
    wait_for(|| {
        surveys.complete(&f.engine, &mut f.model);
        !surveys.busy()
    });
    assert_eq!(f.transport.discoveries.load(Ordering::Relaxed), before + 2);
    assert!(!f.model.rec.schedule.due(Instant::now()));
    f.finish(surveys);
}

#[test]
fn newer_real_topology_discards_a_held_old_topology_reply() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, Stage::Topology, false);
        let mut surveys = Surveys::default();
        f.start(&mut surveys);
        let surveyed_at = f.model.surveyed_at;
        let leader = f.sim.player("A").unwrap().uuid.clone();
        let member = f.sim.player("B").unwrap().ip;
        set_av_transport_uri(&f.sim.lan(), member, &format!("x-rincon:{leader}"), "")
            .unwrap();
        wait_for(|| {
            pump::turn(&f.engine, &mut f.model);
            surveys.complete(&f.engine, &mut f.model);
            f.model.rec.households[0].groups.len() == 1
        });
        assert!(
            surveys
                .pending
                .as_ref()
                .unwrap()
                .cancelled
                .load(Ordering::Acquire)
        );
        let calls = f.transport.calls.load(Ordering::Relaxed);
        f.release();
        wait_for(|| {
            surveys.complete(&f.engine, &mut f.model);
            !surveys.busy()
        });
        assert_eq!(f.model.rec.households[0].groups.len(), 1);
        assert_eq!(f.model.surveyed_at, surveyed_at);
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
        assert!(!f.model.rec.schedule.due(Instant::now()));
        f.finish(surveys);
    }
}

#[test]
fn dropping_a_blocked_inventory_worker_cancels_followup_requests_without_joining() {
    for stage in [Stage::Discovery, Stage::Description, Stage::Topology] {
        let mut f = Fixture::new(2, stage, false);
        let mut surveys = Surveys::default();
        f.start(&mut surveys);
        let calls = f.transport.calls.load(Ordering::Relaxed);
        let cancelled = Arc::clone(&surveys.pending.as_ref().unwrap().cancelled);
        let at = Instant::now();
        drop(surveys);
        assert!(at.elapsed() < Duration::from_secs(1));
        assert!(cancelled.load(Ordering::Acquire));
        f.release();
        wait_for(|| Arc::strong_count(&f.transport) == 2);
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), calls);
        f.finish(Surveys::default());
    }
}

#[test]
fn the_running_live_loop_keeps_applying_events_and_stops_while_discovery_is_held() {
    for generation in [1, 2] {
        let sim = sim(generation);
        let transport = transport(&sim, Stage::Discovery);
        let release = Release(Arc::clone(&transport.gate));
        let config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
        let live = Live::start_with(transport.clone(), lan(&sim), config);
        assert!(live.wait_ready(WAIT));
        let count = wanted(&live.households()).len();
        wait_for(|| {
            sim.gena_log()
                .iter()
                .filter(|e| {
                    matches!(
                        e.event,
                        GenaEvent::Delivered {
                            seq: 0,
                            status: 200,
                            ..
                        }
                    )
                })
                .count()
                >= count
        });
        let player = sim.player("B").unwrap();
        let id = PlayerId(player.uuid.clone());
        let ip = player.ip;
        // A later event's application proves the delivered initial prefix was
        // consumed before we dispatch the survey (not just delivered to HTTP).
        set_volume(&sim.lan(), ip, 41).unwrap();
        wait_for(|| live.player(&id).is_some_and(|p| p.volume == Some(41)));
        transport.gate.armed.store(true, Ordering::Release);
        live.refresh_soon();
        wait_for(|| transport.gate.entered.load(Ordering::Acquire));
        let before = live.snapshot().surveyed_at;
        set_volume(&sim.lan(), ip, 59).unwrap();
        wait_for(|| live.player(&id).is_some_and(|p| p.volume == Some(59)));
        assert_eq!(live.snapshot().surveyed_at, before);
        assert!(!transport.gate.released.load(Ordering::Acquire));
        let calls = transport.calls.load(Ordering::Relaxed);
        let at = Instant::now();
        live.stop();
        assert!(at.elapsed() < Duration::from_secs(2));
        drop(release);
        wait_for(|| Arc::strong_count(&transport) == 1);
        assert_eq!(transport.calls.load(Ordering::Relaxed), calls);
        sim.shutdown();
    }
}
