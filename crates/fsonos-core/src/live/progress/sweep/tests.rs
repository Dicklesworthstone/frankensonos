use super::*;
use crate::HouseholdState;
use crate::events::Want;
use crate::live::{LiveConfig, Snapshot, pump};
use crate::playback::PlayerPlayback;
use crate::reconcile::Reconciler;
use fsonos_proto::control::{seek_position, set_av_transport_uri, set_mute, set_volume};
use fsonos_proto::gena::Notify;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::SocketAddr;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::time::{Duration, SystemTime};

const WAIT: Duration = Duration::from_secs(10);

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

struct Held {
    inner: Arc<dyn Transport + Send + Sync>,
    hosts: Vec<IpAddr>,
    gate: Arc<Gate>,
    fail_mute: Option<IpAddr>,
    calls: AtomicUsize,
}

impl Transport for Held {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if self.fail_mute == Some(host) && action.trim_matches('"').ends_with("#GetMute") {
            return Err(ProtoError::NotWired("injected mute read failure"));
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if self.hosts.contains(&host) && !self.gate.released.load(Ordering::Acquire) {
            self.gate.entered.fetch_add(1, Ordering::Release);
            let deadline = Instant::now() + WAIT;
            while !self.gate.released.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    return Err(ProtoError::NotWired("held sweep reply timed out"));
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(response)
    }
}

struct Fixture {
    // Release held replies first during unwinding, before network teardown.
    _release: Release,
    sim: SimHandle,
    engine: Engine,
    model: Model,
    transport: Arc<Held>,
    players: Vec<(PlayerId, IpAddr)>,
}

impl Fixture {
    fn new(generation: u8, held: usize, fail_mute: bool) -> Self {
        let hardware = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let rooms = ["A", "B", "C", "D", "E"].map(|name| SimPlayerSpec::new(name, hardware));
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(rooms)
        } else {
            builder.s2(rooms)
        }
        .spawn()
        .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let players: Vec<_> = ["A", "B", "C", "D", "E"]
            .iter()
            .map(|name| {
                let p = sim.player(name).unwrap();
                (PlayerId(p.uuid.clone()), p.ip)
            })
            .collect();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&*lan, players[0].1).unwrap());
        for (_, host) in &players {
            set_av_transport_uri(&*lan, *host, "test:sweep", "").unwrap();
            seek_position(&*lan, *host, 41).unwrap();
            set_volume(&*lan, *host, 37).unwrap();
            set_mute(&*lan, *host, true).unwrap();
        }
        let gate = Arc::new(Gate::default());
        let transport = Arc::new(Held {
            inner: Arc::new(sim.lan()),
            hosts: players.iter().take(held).map(|(_, host)| *host).collect(),
            gate: Arc::clone(&gate),
            fail_mute: fail_mute.then_some(players[0].1),
            calls: AtomicUsize::new(0),
        });
        let now = Instant::now();
        let mut rec = Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), now);
        rec.households.push(house);
        rec.schedule.succeeded(now);
        let local = lan.local_address_toward(players[0].1).unwrap();
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let engine = Engine {
            transport: transport.clone(),
            lan,
            config: LiveConfig::new(vec![players[0].1]),
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            wake: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let model = Model {
            ticked: (now, SystemTime::now()),
            remembered: Vec::new(),
            rec,
            playback: Playback::default(),
            progress: super::super::Watchdog::default(),
            sink: Some(sink),
            surveyed_at: Some(now),
            last_error: None,
            route_checked: now,
            route_moved: None,
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

    fn begin(&mut self) {
        let started = Instant::now();
        assert!(observation::survey(&self.engine, &mut self.model).is_empty());
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    fn only_first(&mut self) {
        self.begin();
        self.model
            .progress
            .sweep
            .queue
            .retain(|target| target.player == self.players[0].0);
    }

    fn state(&self, index: usize) -> Option<PlayerPlayback> {
        self.model.playback.of(&self.players[index].0).cloned()
    }

    fn release(&self) {
        self.transport.gate.released.store(true, Ordering::Release);
    }

    fn idle(&self) -> bool {
        self.model.progress.sweep.queue.is_empty()
            && self.model.progress.sweep.len() + self.model.progress.reads.len() == 0
    }

    fn drive(&mut self) {
        wait_for(|| {
            super::super::recover(&self.engine, &mut self.model);
            self.idle()
        });
    }

    fn finish(mut self) {
        self.release();
        self.engine.stop.store(true, Ordering::Release);
        wait_for(|| {
            super::super::background::complete(&self.engine, &mut self.model);
            complete(&self.engine, &mut self.model);
            self.model.progress.sweep.len() + self.model.progress.reads.len() == 0
        });
        self.model
            .rec
            .subscriptions
            .unsubscribe_all(&*self.engine.lan);
        self.sim.shutdown();
    }
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "survey recovery did not converge"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn notify(source: EventSource, body: &str) -> Notify {
    Notify {
        sid: "uuid:sweep-test".into(),
        seq: 1,
        path: format!("/{source:?}"),
        properties: vec![(
            "LastChange".into(),
            format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
        )],
    }
}

#[test]
fn both_generations_recover_healthy_rooms_while_one_sweep_is_stalled() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, 1, false);
        let before = f.sim.soap_log().len();
        f.begin();
        assert!(start_next(&f.engine, &mut f.model));
        wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
        wait_for(|| {
            super::super::recover(&f.engine, &mut f.model);
            f.state(1).is_some_and(|state| {
                state.position_at(Instant::now()) == Some(41)
                    && state.group_volume == Some(37)
                    && state.play_mode.as_deref() == Some("NORMAL")
            })
        });
        assert!(
            f.state(0).is_none(),
            "the blocked room has not published a partial sample"
        );
        let state = f.state(1).unwrap();
        assert_eq!(state.transport, Some(TransportState::Stopped));
        assert_eq!(state.volume, Some(37));
        assert_eq!(state.mute, Some(true));
        assert_eq!(state.group_volume, Some(37));
        assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
        assert!(f.model.progress.sweep.contains(&f.players[0].0));
        f.release();
        f.drive();
        let log = f.sim.soap_log();
        assert_eq!(log.len() - before, 5 * 9);
        assert!(
            log[before..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        let calls = f.transport.calls.load(Ordering::Relaxed);
        for _ in 0..20 {
            super::super::recover(&f.engine, &mut f.model);
        }
        assert_eq!(
            f.transport.calls.load(Ordering::Relaxed),
            calls,
            "no new polling loop"
        );
        f.finish();
    }
}

#[test]
fn newer_equal_room_events_reject_only_the_stale_service() {
    let mut f = Fixture::new(2, 0, false);
    let player = f.players[0].0.clone();
    f.model.playback.apply_room_levels(&player, 10, false);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| f.model.progress.sweep.pending[0].thread.is_finished());
    for volume in [20, 10] {
        let source = EventSource::RenderingControl;
        let body = format!("<Volume channel=\"Master\" val=\"{volume}\"/>");
        f.model
            .playback
            .apply(&player, source, &notify(source, &body), Instant::now())
            .unwrap();
    }
    complete(&f.engine, &mut f.model);
    let state = f.state(0).unwrap();
    assert_eq!(state.volume, Some(10));
    assert_eq!(state.mute, Some(false));
    assert_eq!(state.position_at(Instant::now()), Some(41));
    assert_eq!(state.group_volume, Some(37));
    assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
    f.finish();
}

#[test]
fn one_failed_service_does_not_erase_its_old_values_or_hide_other_services() {
    let mut f = Fixture::new(2, 0, true);
    f.model
        .playback
        .apply_room_levels(&f.players[0].0, 11, false);
    f.begin();
    f.drive();
    let first = f.state(0).unwrap();
    assert_eq!(
        first.volume,
        Some(11),
        "GetVolume alone cannot replace a room-level pair"
    );
    assert_eq!(first.mute, Some(false));
    assert_eq!(first.position_at(Instant::now()), Some(41));
    assert_eq!(first.play_mode.as_deref(), Some("NORMAL"));
    assert_eq!(f.state(1).unwrap().volume, Some(37));
    f.finish();
}

#[test]
fn sweep_and_clock_workers_share_one_bound_and_one_slot_per_player() {
    let mut f = Fixture::new(2, 5, false);
    let player = f.players[0].0.clone();
    let source = EventSource::AvTransport;
    let body = "<TransportState val=\"STOPPED\"/><CurrentTrackURI val=\"test:sweep\"/>";
    f.model
        .playback
        .apply(&player, source, &notify(source, body), Instant::now())
        .unwrap();
    super::super::recover(&f.engine, &mut f.model);
    wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
    assert_eq!(f.model.progress.reads.len(), 1);
    f.begin();
    for _ in 0..20 {
        super::super::recover(&f.engine, &mut f.model);
    }
    wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 4);
    for _ in 0..100 {
        request(&mut f.model);
        super::super::recover(&f.engine, &mut f.model);
    }
    assert_eq!(
        f.model.progress.reads.len() + f.model.progress.sweep.len(),
        4
    );
    assert_eq!(f.transport.calls.load(Ordering::Relaxed), 4);
    assert!(f.model.progress.sweep.queue.len() <= f.players.len());
    assert!(
        !f.model.progress.sweep.contains(&player),
        "the clock already owns this player"
    );
    f.release();
    f.drive();
    assert!(
        f.players
            .iter()
            .all(|(id, _)| f.model.playback.of(id).is_some())
    );
    f.finish();
}

#[test]
fn shutdown_does_not_publish_or_continue_a_blocked_sweep() {
    let mut f = Fixture::new(2, 1, false);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
    f.engine.stop.store(true, Ordering::Release);
    let started = Instant::now();
    super::super::recover(&f.engine, &mut f.model);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert!(
        f.model.progress.sweep.pending[0]
            .cancelled
            .load(Ordering::Acquire)
    );
    f.release();
    wait_for(|| {
        complete(&f.engine, &mut f.model);
        f.model.progress.sweep.is_empty()
    });
    assert_eq!(f.transport.calls.load(Ordering::Relaxed), 1);
    assert!(f.state(0).is_none());
    assert!(
        f.engine
            .snapshot
            .lock()
            .unwrap()
            .playback
            .of(&f.players[0].0)
            .is_none()
    );
    f.finish();
}

#[test]
fn a_changed_group_or_address_rejects_the_old_renderer_sample() {
    for change_address in [false, true] {
        let mut f = Fixture::new(2, 0, false);
        f.only_first();
        assert!(start_next(&f.engine, &mut f.model));
        wait_for(|| f.model.progress.sweep.pending[0].thread.is_finished());
        let player = f.players[0].0.clone();
        let house = &mut f.model.rec.households[0];
        if change_address {
            house
                .players
                .iter_mut()
                .find(|p| p.id == player)
                .unwrap()
                .ip = "192.0.2.200".parse().unwrap();
        } else {
            house.groups.retain(|group| group.coordinator != player);
        }
        complete(&f.engine, &mut f.model);
        assert!(f.state(0).is_none());
        f.finish();
    }
}

#[test]
fn the_actual_event_pump_still_applies_notifications_during_a_slow_sweep() {
    let mut f = Fixture::new(2, 1, false);
    let (player, host) = f.players[1].clone();
    let sink = f.model.sink.as_ref().unwrap();
    let report = f.model.rec.subscriptions.sync(
        &*f.engine.lan,
        &[Want {
            player: player.clone(),
            host,
            service: Service::RenderingControl,
        }],
        |service| sink.callback_url(service.tag()),
        Instant::now(),
    );
    assert_eq!(report.subscribed, 1);
    wait_for(|| {
        pump::turn(&f.engine, &mut f.model);
        f.state(1).is_some_and(|state| state.volume == Some(37))
    });
    f.begin();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
    set_volume(&*f.engine.lan, host, 59).unwrap();
    wait_for(|| {
        f.sim.gena_log().iter().any(|entry| {
            matches!(
                entry.event,
                GenaEvent::Delivered {
                    seq: 1,
                    status: 200,
                    ..
                }
            )
        })
    });
    let started = Instant::now();
    pump::turn(&f.engine, &mut f.model);
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(f.state(1).unwrap().volume, Some(59));
    assert!(!f.transport.gate.released.load(Ordering::Acquire));
    assert!(f.model.progress.sweep.contains(&f.players[0].0));
    f.finish();
}

#[test]
fn group_members_only_read_their_own_levels_on_both_generations() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, 0, false);
        let leader = &f.players[0].0;
        set_av_transport_uri(
            &*f.engine.lan,
            f.players[1].1,
            &format!("x-rincon:{}", leader.0),
            "",
        )
        .unwrap();
        let topology = get_zone_group_state(&*f.engine.lan, f.players[0].1).unwrap();
        f.model.rec.households[0].apply_topology(&topology);
        assert_eq!(f.model.rec.households[0].groups.len(), 4);
        let before = f.sim.soap_log().len();
        f.begin();
        f.drive();
        let member = f.state(1).unwrap();
        assert_eq!(member.volume, Some(37));
        assert_eq!(member.mute, Some(true));
        assert!(member.transport.is_none());
        assert!(member.group_volume.is_none());
        assert!(member.play_mode.is_none());
        let log = f.sim.soap_log();
        let member_reads: Vec<_> = log[before..]
            .iter()
            .filter(|call| call.room == "B")
            .map(|call| call.action.as_str())
            .collect();
        assert_eq!(member_reads, ["GetVolume", "GetMute"]);
        assert_eq!(log.len() - before, 4 * 9 + 2);
        assert!(
            log[before..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        f.finish();
    }
}

#[test]
fn returning_to_the_original_group_never_revives_an_invalidated_read() {
    for clock in [false, true] {
        let mut f = Fixture::new(2, 1, false);
        let player = f.players[0].0.clone();
        if clock {
            let source = EventSource::AvTransport;
            let body = "<TransportState val=\"STOPPED\"/><CurrentTrackURI val=\"test:sweep\"/>";
            f.model
                .playback
                .apply(&player, source, &notify(source, body), Instant::now())
                .unwrap();
            super::super::recover(&f.engine, &mut f.model);
        } else {
            f.only_first();
            assert!(start_next(&f.engine, &mut f.model));
        }
        wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
        let before = f.model.rec.households[0].clone();
        let member = f.players[1].0.clone();
        let house = &mut f.model.rec.households[0];
        house.groups.retain(|group| group.coordinator != member);
        house
            .groups
            .iter_mut()
            .find(|group| group.coordinator == player)
            .unwrap()
            .members
            .push(member);
        // This hook runs after every consumed notification, not only once
        // after the complete burst has returned to its original topology.
        super::super::invalidate(&f.model);
        f.model.rec.households[0] = before;
        super::super::invalidate(&f.model);
        f.release();
        wait_for(|| {
            super::super::background::complete(&f.engine, &mut f.model);
            complete(&f.engine, &mut f.model);
            f.model.progress.reads.len() + f.model.progress.sweep.len() == 0
        });
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), 1);
        assert!(
            f.state(0)
                .is_none_or(|state| state.position_at(Instant::now()).is_none())
        );
        f.finish();
    }
}

#[test]
fn a_completed_sweep_stays_invalid_after_an_offline_player_returns() {
    let mut f = Fixture::new(2, 0, false);
    f.only_first();
    assert!(start_next(&f.engine, &mut f.model));
    wait_for(|| f.model.progress.sweep.pending[0].thread.is_finished());
    let player = f.players[0].0.clone();
    f.model
        .rec
        .health
        .missing(&player, "test offline transition");
    super::super::invalidate(&f.model);
    f.model.rec.health.ok(&player, Instant::now());
    super::super::invalidate(&f.model);
    complete(&f.engine, &mut f.model);
    assert!(f.state(0).is_none());
    assert!(f.model.progress.sweep.pending.is_empty());
    f.finish();
}

mod eventless;
mod streaming;

#[test]
fn bootstrap_readiness_counts_failed_services_as_finished_attempts() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, 0, true);
        f.model.surveyed_at = None;
        super::super::request_survey(&mut f.model);
        f.model.surveyed_at = Some(Instant::now());
        assert!(!super::super::finish_bootstrap(&mut f.model));
        f.drive();
        assert!(super::super::finish_bootstrap(&mut f.model));
        let state = f.state(0).unwrap();
        assert!(state.volume.is_none());
        assert!(state.mute.is_none());
        assert_eq!(state.position_at(Instant::now()), Some(41));
        assert_eq!(state.group_volume, Some(37));
        assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
        f.finish();
    }
}

#[test]
fn bootstrap_readiness_retains_bounded_workers_until_they_really_finish() {
    let mut f = Fixture::new(2, 4, false);
    f.model.surveyed_at = None;
    super::super::request_survey(&mut f.model);
    f.model.surveyed_at = Some(Instant::now());
    wait_for(|| {
        super::super::recover(&f.engine, &mut f.model);
        f.transport.gate.entered.load(Ordering::Acquire) == 4
    });
    for _ in 0..100 {
        super::super::refresh_bootstrap(&mut f.model);
        super::super::recover(&f.engine, &mut f.model);
        assert!(!super::super::finish_bootstrap(&mut f.model));
    }
    assert_eq!(f.model.progress.sweep.pending.len(), 4);
    assert_eq!(f.model.progress.sweep.queue.len(), 1);
    assert_eq!(f.transport.calls.load(Ordering::Relaxed), 4);
    assert!(f.state(4).is_none());
    f.release();
    f.drive();
    assert!(super::super::finish_bootstrap(&mut f.model));
    assert!(f.state(4).is_some());
    f.finish();
}

#[test]
fn same_address_reboot_cancels_both_worker_kinds_before_fresh_events() {
    for clock in [false, true] {
        let mut f = Fixture::new(2, 1, false);
        let player = f.players[0].0.clone();
        if clock {
            let source = EventSource::AvTransport;
            let body = "<TransportState val=\"STOPPED\"/><CurrentTrackURI val=\"test:sweep\"/>";
            f.model
                .playback
                .apply(&player, source, &notify(source, body), Instant::now())
                .unwrap();
            super::super::recover(&f.engine, &mut f.model);
        } else {
            f.only_first();
            assert!(start_next(&f.engine, &mut f.model));
        }
        wait_for(|| f.transport.gate.entered.load(Ordering::Acquire) == 1);
        let topology = f.model.rec.households.clone();
        super::super::invalidate_rebooted(&f.model, std::slice::from_ref(&player));
        super::super::request_survey(&mut f.model);
        f.model
            .progress
            .sweep
            .queue
            .retain(|target| target.player == player);
        assert_eq!(f.model.rec.households, topology);
        assert_eq!(f.model.progress.sweep.queue.len(), 1);
        assert!(!start_next(&f.engine, &mut f.model));
        f.release();
        wait_for(|| {
            super::super::background::complete(&f.engine, &mut f.model);
            complete(&f.engine, &mut f.model);
            f.model.progress.reads.len() + f.model.progress.sweep.len() == 0
        });
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), 1);
        assert!(
            f.state(0)
                .is_none_or(|p| p.position_at(Instant::now()).is_none())
        );
        f.drive();
        assert_eq!(f.state(0).unwrap().position_at(Instant::now()), Some(41));
        assert_eq!(f.transport.calls.load(Ordering::Relaxed), 10);
        f.finish();
    }
}
