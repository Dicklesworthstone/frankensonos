//! The callback listener is optional for discovery, never for claiming GENA.

use super::*;
use crate::events::wanted;
use fsonos_proto::control::{pause, play, set_av_transport_uri, set_mute, set_volume};
use fsonos_proto::play_mode::set_play_mode;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, TransportState};
use std::net::TcpListener;
use std::sync::atomic::AtomicUsize;

fn fixture() -> (SimHandle, Arc<Lan>, LiveConfig, TcpListener) {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Legacy", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Modern", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    );
    let local = lan.local_address_toward(sim.players()[0].ip).unwrap();
    assert!(local.is_loopback(), "the fixture must not bind a real LAN");
    let occupied = TcpListener::bind(SocketAddr::new(local, 0)).unwrap();
    let mut config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
    config.callback_port = occupied.local_addr().unwrap().port();
    (sim, lan, config, occupied)
}

fn engine(sim: &SimHandle, lan: Arc<Lan>, config: LiveConfig) -> Engine {
    Engine {
        transport: Arc::new(sim.lan()),
        lan,
        config,
        snapshot: Arc::new(Mutex::new(Snapshot::default())),
        subscribers: Arc::new(Mutex::new(Vec::new())),
        wake: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
    }
}

fn model(config: &LiveConfig) -> Model {
    let now = Instant::now();
    Model {
        ticked: (now, SystemTime::now()),
        remembered: config.remembered.clone(),
        rec: Reconciler::new(config.interval, config.max_backoff, now),
        playback: Playback::default(),
        progress: progress::Watchdog::default(),
        sink: None,
        surveyed_at: None,
        last_error: None,
        route_checked: now,
        route_moved: None,
    }
}

fn survey(engine: &Engine, model: &mut Model) {
    let events = engine.survey(model, Instant::now());
    engine.publish(model, events);
}

#[test]
fn an_occupied_callback_port_does_not_hide_reachable_households_or_controls() {
    let (sim, lan, mut config, _occupied) = fixture();
    let remembered = Arc::new(AtomicUsize::new(0));
    let recorded = Arc::clone(&remembered);
    config.surveyed = Some(Arc::new(move |houses| {
        recorded.store(houses.len(), Ordering::Release);
    }));
    // Exercise the cached-only seed path as well as a local bind failure.
    config.remembered = std::mem::take(&mut config.seeds);
    let engine = engine(&sim, lan, config);
    let mut model = model(&engine.config);
    let (send, received) = sync_channel(64);
    engine
        .subscribers
        .lock()
        .unwrap()
        .push(delivery::Pending::new(send));
    for name in ["Legacy", "Modern"] {
        let host = sim.player(name).unwrap().ip;
        set_av_transport_uri(&sim.lan(), host, "test:radio", "").unwrap();
        play(&sim.lan(), host).unwrap();
        pause(&sim.lan(), host).unwrap();
        set_volume(&sim.lan(), host, 23).unwrap();
        set_mute(&sim.lan(), host, true).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::from_wire("REPEAT_ALL").unwrap()).unwrap();
    }
    let calls = sim.soap_log().len();
    survey(&engine, &mut model);
    let snapshot = engine.snapshot.lock().unwrap().clone();
    assert_eq!(snapshot.households.len(), 2);
    assert_eq!(remembered.load(Ordering::Acquire), 2);
    assert!(model.remembered.is_empty(), "observed startup hints retire");
    assert!(snapshot.surveyed_at.is_some());
    assert_eq!(snapshot.callback, None);
    assert_eq!(snapshot.subscriptions, 0);
    assert!(
        snapshot
            .last_error
            .as_deref()
            .unwrap()
            .contains("cannot listen")
    );
    assert!(
        sim.gena_log().is_empty(),
        "no bogus callback was subscribed"
    );
    for name in ["Legacy", "Modern"] {
        let id = PlayerId(sim.player(name).unwrap().uuid.clone());
        let state = snapshot.playback.of(&id).unwrap();
        assert_eq!(state.transport, Some(TransportState::Paused));
        assert_eq!(state.track_uri.as_deref(), Some("test:radio"));
        assert_eq!((state.volume, state.mute), (Some(23), Some(true)));
        assert_eq!(state.play_mode.as_deref(), Some("REPEAT_ALL"));
        assert!(state.group_volume.is_some());
        assert_eq!(snapshot.health.of(&id).unwrap().health, Health::Healthy);
    }
    assert!(
        sim.soap_log()[calls..]
            .iter()
            .all(|call| call.action.starts_with("Get"))
    );
    let events: Vec<_> = received.try_iter().collect();
    assert!(events.contains(&LiveEvent::Topology));
    assert!(
        events
            .iter()
            .any(|e| matches!(e, LiveEvent::Playback { .. }))
    );
    let next = model.rec.schedule.next_at();
    assert!(next > Instant::now());
    assert!(next <= Instant::now() + EVENTLESS_RETRY);
    // Even a slow preceding pass gets a fresh cooldown, not an overdue loop.
    let old = Instant::now()
        .checked_sub(engine.config.interval * 2)
        .unwrap();
    let events = engine.survey(&mut model, old);
    engine.publish(&model, events);
    assert!(model.rec.schedule.next_at() > Instant::now());
    assert_eq!(model.rec.subscriptions.len(), 0);
    engine.release(&mut model);
    sim.shutdown();
}

#[test]
fn releasing_the_requested_port_restores_subscriptions_and_event_driven_updates() {
    let (sim, lan, config, occupied) = fixture();
    let port = config.callback_port;
    let engine = engine(&sim, lan, config);
    let mut model = model(&engine.config);
    survey(&engine, &mut model);
    assert_eq!(model.rec.households.len(), 2);
    assert!(model.sink.is_none());
    assert!(model.last_error.is_some());
    drop(occupied);
    survey(&engine, &mut model);
    assert_eq!(model.sink.as_ref().unwrap().local_addr().port(), port);
    assert_eq!(
        model.rec.subscriptions.len(),
        wanted(&model.rec.households).len()
    );
    assert_eq!(model.rec.subscriptions.len(), 8);
    assert!(model.last_error.is_none());
    assert!(model.rec.schedule.next_at() > Instant::now() + EVENTLESS_RETRY);
    let snapshot = engine.snapshot.lock().unwrap().clone();
    assert!(
        snapshot
            .callback
            .as_deref()
            .unwrap()
            .ends_with(&format!(":{port}"))
    );
    assert!(snapshot.last_error.is_none());
    let host = sim.player("Modern").unwrap().ip;
    let id = PlayerId(sim.player("Modern").unwrap().uuid.clone());
    set_volume(&sim.lan(), host, 47).unwrap();
    let calls = sim.soap_log().len();
    let until = Instant::now() + Duration::from_secs(5);
    while model.playback.of(&id).unwrap().volume != Some(47) {
        assert!(
            Instant::now() < until,
            "recovered listener did not deliver the change"
        );
        pump::turn(&engine, &mut model);
    }
    assert_eq!(
        sim.soap_log().len(),
        calls,
        "GENA, not polling, delivered the update"
    );
    assert_eq!(
        engine
            .snapshot
            .lock()
            .unwrap()
            .playback
            .of(&id)
            .unwrap()
            .volume,
        Some(47)
    );
    engine.release(&mut model);
    sim.shutdown();
}

struct Unreachable;

impl Transport for Unreachable {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        Err(ProtoError::NotWired("test network unavailable"))
    }

    fn http_get(&self, _: &str) -> Result<String, ProtoError> {
        Err(ProtoError::NotWired("test network unavailable"))
    }
}

#[test]
fn failed_discovery_keeps_the_cached_model_and_reports_both_failures() {
    let (sim, lan, config, _occupied) = fixture();
    let mut engine = engine(&sim, lan, config);
    let mut model = model(&engine.config);
    survey(&engine, &mut model);
    let previous = engine.snapshot.lock().unwrap().clone();
    assert_eq!(previous.households.len(), 2);
    engine.transport = Arc::new(Unreachable);
    survey(&engine, &mut model);
    let snapshot = engine.snapshot.lock().unwrap().clone();
    assert_eq!(snapshot.households, previous.households);
    assert_eq!(snapshot.surveyed_at, previous.surveyed_at);
    let detail = snapshot.last_error.unwrap();
    assert!(detail.contains("cannot listen"), "{detail}");
    assert!(detail.contains("survey failed"), "{detail}");
    assert_eq!(snapshot.subscriptions, 0);
    assert_eq!(snapshot.callback, None);
    assert!(model.rec.schedule.next_at() > Instant::now());
    for p in sim.players() {
        let id = PlayerId(p.uuid.clone());
        assert_eq!(snapshot.playback.of(&id), previous.playback.of(&id));
    }
    engine.release(&mut model);
    sim.shutdown();
}

#[test]
fn shutdown_does_not_try_opening_a_listener_or_surveying() {
    let (sim, lan, config, _occupied) = fixture();
    let engine = engine(&sim, lan, config);
    let mut model = model(&engine.config);
    engine.stop.store(true, Ordering::Release);
    let calls = sim.soap_log().len();
    assert!(engine.survey(&mut model, Instant::now()).is_empty());
    assert!(model.rec.households.is_empty());
    assert!(model.sink.is_none());
    assert!(model.last_error.is_none());
    assert_eq!(sim.soap_log().len(), calls);
    assert!(sim.gena_log().is_empty());
    sim.shutdown();
}

#[test]
fn the_real_live_worker_becomes_ready_without_a_callback_and_can_recover_it() {
    let (sim, lan, config, occupied) = fixture();
    let live = Live::start_with(Arc::new(sim.lan()), lan, config);
    assert!(live.wait_ready(Duration::from_secs(10)));
    assert_eq!(live.households().len(), 2);
    assert!(live.snapshot().callback.is_none());
    drop(occupied);
    live.refresh_soon();
    let until = Instant::now() + Duration::from_secs(10);
    loop {
        let snapshot = live.snapshot();
        if snapshot.callback.is_some() && snapshot.subscriptions == 8 {
            assert!(snapshot.last_error.is_none());
            break;
        }
        assert!(
            Instant::now() < until,
            "live worker did not recover the callback"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    live.stop();
    sim.shutdown();
}
