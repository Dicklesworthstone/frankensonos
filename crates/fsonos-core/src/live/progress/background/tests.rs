use super::*;
use crate::HouseholdState;
use crate::live::{LiveConfig, Snapshot};
use crate::playback::{EventSource, Playback};
use crate::reconcile::Reconciler;
use fsonos_proto::Transport;
use fsonos_proto::control::{seek_position, set_av_transport_uri};
use fsonos_proto::gena::Notify;
use fsonos_proto::net::Lan;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime};

const WAIT: Duration = Duration::from_secs(5);

#[derive(Default)]
struct Gate {
    entered: AtomicUsize,
    released: AtomicBool,
}

struct Release(Arc<Gate>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.released.store(true, Ordering::Release);
    }
}

/// Hold a real SOAP reply, not the live-model thread. Every other call goes
/// through the simulator's actual localhost HTTP endpoints.
struct HeldReply {
    inner: SimLan,
    hosts: Vec<IpAddr>,
    gate: Arc<Gate>,
    calls: AtomicUsize,
    fail_once: AtomicBool,
}

impl Transport for HeldReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_once.swap(false, Ordering::AcqRel) {
            return Err(ProtoError::NotWired("injected read failure"));
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if self.hosts.contains(&host) && !self.gate.released.load(Ordering::Acquire) {
            self.gate.entered.fetch_add(1, Ordering::Release);
            let deadline = Instant::now() + WAIT;
            while !self.gate.released.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err(ProtoError::NotWired("held reply timed out"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(response)
    }
}

struct Fixture {
    // Release before dropping network fixtures, including during unwinding.
    _release: Release,
    sim: SimHandle,
    engine: Engine,
    model: Model,
    transport: Arc<HeldReply>,
    players: Vec<(PlayerId, IpAddr)>,
}

impl Fixture {
    fn new(generation: u8, hold_all: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = match generation {
            1 => builder.s1([
                SimPlayerSpec::new("A", SimModel::Play5Gen1),
                SimPlayerSpec::new("B", SimModel::Play5Gen1),
                SimPlayerSpec::new("C", SimModel::Play5Gen1),
                SimPlayerSpec::new("D", SimModel::Play5Gen1),
                SimPlayerSpec::new("E", SimModel::Play5Gen1),
            ]),
            _ => builder.s2([
                SimPlayerSpec::new("A", SimModel::One),
                SimPlayerSpec::new("B", SimModel::One),
                SimPlayerSpec::new("C", SimModel::One),
                SimPlayerSpec::new("D", SimModel::One),
                SimPlayerSpec::new("E", SimModel::One),
            ]),
        }
        .spawn()
        .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let host = sim.player("A").unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&*lan, host).unwrap());
        let players: Vec<_> = house
            .players
            .iter()
            .map(|player| (player.id.clone(), player.ip))
            .collect();
        assert_eq!(house.groups.len(), 5);
        let mut playback = Playback::default();
        for (player, address) in &players {
            set_av_transport_uri(&*lan, *address, "test:clock", "").unwrap();
            seek_position(&*lan, *address, 41).unwrap();
            request(&mut playback, player);
        }
        let gate = Arc::new(Gate::default());
        let transport = Arc::new(HeldReply {
            inner: sim.lan(),
            hosts: if hold_all {
                players.iter().map(|(_, host)| *host).collect()
            } else {
                vec![players[0].1]
            },
            gate: Arc::clone(&gate),
            calls: AtomicUsize::new(0),
            fail_once: AtomicBool::new(false),
        });
        let now = Instant::now();
        let mut rec = Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), now);
        rec.households.push(house);
        let engine = Engine {
            transport: transport.clone(),
            lan,
            config: LiveConfig::new(vec![host]),
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            wake: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let model = Model {
            ticked: (now, SystemTime::now()),
            remembered: Vec::new(),
            rec,
            playback,
            progress: super::super::Watchdog::default(),
            sink: None,
            surveyed_at: Some(now),
            last_error: None,
            route_checked: now,
        };
        Self {
            _release: Release(gate),
            sim,
            engine,
            model,
            transport,
            players,
        }
    }

    fn launch(&mut self, index: usize) {
        let target = &self.players[index..=index];
        let probe = self
            .model
            .progress
            .next(target, &self.model.playback, Instant::now())
            .unwrap();
        start(&self.engine, &mut self.model, probe);
    }

    fn release(&self) {
        self.transport.gate.released.store(true, Ordering::Release);
    }

    fn drain(&mut self) {
        let deadline = Instant::now() + WAIT;
        while !self.model.progress.reads.pending.is_empty() {
            complete(&self.engine, &mut self.model);
            assert!(Instant::now() < deadline, "read workers did not finish");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    fn finish(mut self) {
        self.release();
        self.drain();
        self.sim.shutdown();
    }
}

fn request(playback: &mut Playback, player: &PlayerId) {
    let notify = Notify {
        sid: "uuid:background-test".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            "<Event><InstanceID val=\"0\"><TransportState val=\"STOPPED\"/>\
             <CurrentTrackURI val=\"test:clock\"/></InstanceID></Event>"
                .into(),
        )],
    };
    playback
        .apply(player, EventSource::AvTransport, &notify, Instant::now())
        .unwrap();
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "background test did not converge"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn a_slow_player_does_not_hold_up_another_rooms_clock_on_either_generation() {
    for generation in [1, 2] {
        let mut fixture = Fixture::new(generation, false);
        let started = Instant::now();
        fixture.launch(0);
        assert!(started.elapsed() < Duration::from_secs(1));
        wait_for(|| fixture.transport.gate.entered.load(Ordering::Acquire) == 1);
        fixture.launch(1);
        let player = fixture.players[1].0.clone();
        wait_for(|| {
            complete(&fixture.engine, &mut fixture.model);
            fixture
                .model
                .playback
                .of(&player)
                .unwrap()
                .position_at(Instant::now())
                == Some(41)
        });
        assert!(fixture.model.progress.reads.contains(&fixture.players[0].0));
        let snapshot = fixture.engine.snapshot.lock().unwrap();
        assert_eq!(
            snapshot
                .playback
                .of(&player)
                .unwrap()
                .position_at(Instant::now()),
            Some(41)
        );
        drop(snapshot);
        assert!(
            fixture
                .sim
                .soap_log()
                .iter()
                .rev()
                .take(6)
                .all(|call| call.action.starts_with("Get")),
            "recovery must never change the music"
        );
        fixture.finish();
    }
}

#[test]
fn cancelled_workers_keep_their_capacity_and_single_flight_until_they_exit() {
    let mut fixture = Fixture::new(2, true);
    for index in 0..MAX_READS {
        fixture.launch(index);
    }
    wait_for(|| fixture.transport.gate.entered.load(Ordering::Acquire) == MAX_READS);
    let player = fixture.players[0].0.clone();
    request(&mut fixture.model.playback, &player);
    complete(&fixture.engine, &mut fixture.model);
    assert!(
        fixture.model.progress.reads.pending[0]
            .cancelled
            .load(Ordering::Acquire)
    );
    for _ in 0..10 {
        super::super::recover(&fixture.engine, &mut fixture.model);
    }
    assert_eq!(fixture.model.progress.reads.pending.len(), MAX_READS);
    assert_eq!(fixture.transport.calls.load(Ordering::Relaxed), MAX_READS);
    assert!(
        fixture
            .model
            .progress
            .next(
                &fixture.players[..1],
                &fixture.model.playback,
                Instant::now()
            )
            .is_none()
    );
    fixture.release();
    fixture.drain();
    fixture.launch(MAX_READS);
    fixture.drain();
    assert_eq!(
        fixture
            .model
            .playback
            .of(&fixture.players[MAX_READS].0)
            .unwrap()
            .position_at(Instant::now()),
        Some(41)
    );
    fixture.finish();
}

#[test]
fn an_identical_seek_event_fences_a_finished_but_unpublished_sample() {
    let mut fixture = Fixture::new(2, false);
    fixture.launch(0);
    fixture.release();
    wait_for(|| fixture.model.progress.reads.pending[0].thread.is_finished());
    let player = fixture.players[0].0.clone();
    // Same URI, state and slot: only the re-armed clock request differs.
    request(&mut fixture.model.playback, &player);
    let before = fixture.model.playback.of(&player).unwrap().clone();
    complete(&fixture.engine, &mut fixture.model);
    assert_eq!(fixture.model.playback.of(&player), Some(&before));
    assert!(before.needs_position_sample());
    assert!(
        fixture
            .engine
            .snapshot
            .lock()
            .unwrap()
            .playback
            .of(&player)
            .is_none()
    );
    // A later request is admitted after normal coalescing, without a failure
    // penalty or a second GENA event, and then publishes the real clock.
    let probe = fixture
        .model
        .progress
        .next(
            &fixture.players[..1],
            &fixture.model.playback,
            Instant::now() + super::super::SAMPLE_INTERVAL,
        )
        .unwrap();
    start(&fixture.engine, &mut fixture.model, probe);
    fixture.drain();
    assert_eq!(
        fixture
            .model
            .playback
            .of(&player)
            .unwrap()
            .position_at(Instant::now()),
        Some(41)
    );
    fixture.finish();
}

#[test]
fn removed_players_and_changed_coordinators_cannot_accept_old_results() {
    for remove in [false, true] {
        let mut fixture = Fixture::new(2, false);
        fixture.launch(0);
        fixture.release();
        wait_for(|| fixture.model.progress.reads.pending[0].thread.is_finished());
        let player = fixture.players[0].0.clone();
        if remove {
            fixture.model.playback.remove(&player);
        } else {
            fixture.model.rec.households[0]
                .groups
                .retain(|group| group.coordinator != player);
        }
        complete(&fixture.engine, &mut fixture.model);
        if remove {
            assert!(fixture.model.playback.of(&player).is_none());
        } else {
            assert_eq!(
                fixture
                    .model
                    .playback
                    .of(&player)
                    .unwrap()
                    .position_at(Instant::now()),
                None
            );
        }
        assert!(
            fixture
                .engine
                .snapshot
                .lock()
                .unwrap()
                .playback
                .of(&player)
                .is_none()
        );
        fixture.finish();
    }
}

#[test]
fn failed_reads_rearm_the_request_and_retry_after_backoff_without_new_events() {
    let mut fixture = Fixture::new(2, false);
    fixture.release();
    fixture.transport.fail_once.store(true, Ordering::Release);
    fixture.launch(0);
    fixture.drain();
    let player = fixture.players[0].0.clone();
    assert!(
        fixture
            .model
            .playback
            .of(&player)
            .unwrap()
            .needs_position_sample()
    );
    assert_eq!(fixture.model.progress.attempts[&player].failures, 1);
    assert!(
        fixture
            .model
            .progress
            .next(
                &fixture.players[..1],
                &fixture.model.playback,
                Instant::now()
            )
            .is_none()
    );
    let due = fixture.model.progress.attempts[&player].retry_at;
    let probe = fixture
        .model
        .progress
        .next(&fixture.players[..1], &fixture.model.playback, due)
        .unwrap();
    start(&fixture.engine, &mut fixture.model, probe);
    fixture.drain();
    assert_eq!(fixture.model.progress.attempts[&player].failures, 0);
    assert_eq!(
        fixture
            .model
            .playback
            .of(&player)
            .unwrap()
            .position_at(Instant::now()),
        Some(41)
    );
    fixture.finish();
}

#[test]
fn shutdown_and_drop_do_not_join_blocked_reads_or_send_followup_requests() {
    for drop_reads in [false, true] {
        let mut fixture = Fixture::new(2, false);
        fixture.launch(0);
        wait_for(|| fixture.transport.gate.entered.load(Ordering::Acquire) == 1);
        let started = Instant::now();
        if drop_reads {
            let reads = std::mem::take(&mut fixture.model.progress.reads);
            let cancellation = Arc::clone(&reads.pending[0].cancelled);
            drop(reads);
            assert!(cancellation.load(Ordering::Acquire));
        } else {
            fixture.engine.stop.store(true, Ordering::Release);
            super::super::recover(&fixture.engine, &mut fixture.model);
            assert!(
                fixture.model.progress.reads.pending[0]
                    .cancelled
                    .load(Ordering::Acquire)
            );
        }
        assert!(started.elapsed() < Duration::from_secs(1));
        fixture.release();
        fixture.drain();
        assert_eq!(fixture.transport.calls.load(Ordering::Relaxed), 1);
        assert!(
            fixture
                .engine
                .snapshot
                .lock()
                .unwrap()
                .playback
                .of(&fixture.players[0].0)
                .is_none()
        );
        fixture.finish();
    }
}

#[test]
fn volume_and_mute_changes_do_not_starve_clock_recovery_or_get_overwritten() {
    let mut fixture = Fixture::new(2, false);
    fixture.launch(0);
    wait_for(|| fixture.transport.gate.entered.load(Ordering::Acquire) == 1);
    let player = fixture.players[0].0.clone();
    let notify = Notify {
        sid: "uuid:rendering-test".into(),
        seq: 1,
        path: "/RenderingControl".into(),
        properties: vec![(
            "LastChange".into(),
            "<Event><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"77\"/>\
             <Mute channel=\"Master\" val=\"1\"/></InstanceID></Event>"
                .into(),
        )],
    };
    fixture
        .model
        .playback
        .apply(
            &player,
            EventSource::RenderingControl,
            &notify,
            Instant::now(),
        )
        .unwrap();
    complete(&fixture.engine, &mut fixture.model);
    assert!(fixture.model.progress.reads.pending[0].current(&fixture.model));
    fixture.release();
    fixture.drain();
    let state = fixture.model.playback.of(&player).unwrap();
    assert_eq!(state.position_at(Instant::now()), Some(41));
    assert_eq!(state.volume, Some(77));
    assert_eq!(state.mute, Some(true));
    assert!(!state.needs_position_sample());
    fixture.finish();
}

mod backlog;
