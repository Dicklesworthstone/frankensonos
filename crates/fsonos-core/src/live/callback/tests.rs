use super::*;
use crate::live::{LiveConfig, Snapshot, delivery};
use crate::playback::Playback;
use crate::reconcile::Reconciler;
use fsonos_proto::control::set_volume;
use fsonos_proto::net::{EVENT_BACKLOG, Lan};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

const WAIT: Duration = Duration::from_secs(20);
const CLIP: &[u8] = b"synthetic media survives callback overload";

fn fixture(generation: u8) -> (SimHandle, Engine, Model, tempfile::TempDir) {
    let builder = SimHousehold::builder();
    let sim = if generation == 1 {
        builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
    }
    .spawn()
    .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
    );
    let media = tempfile::tempdir().unwrap();
    let path = media.path().join("clip.wav");
    std::fs::write(&path, CLIP).unwrap();
    let mut config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
    config.media = Some(Arc::new(move |name| (name == "clip.wav").then(|| path.clone())));
    let now = Instant::now();
    let mut model = Model {
        ticked: (now, SystemTime::now()),
        remembered: Vec::new(),
        rec: Reconciler::new(config.interval, config.max_backoff, now),
        playback: Playback::default(),
        progress: progress::Watchdog::default(),
        sink: None,
        surveyed_at: None,
        last_error: None,
        route_checked: now,
    };
    let engine = Engine {
        transport: Arc::new(sim.lan()),
        lan,
        config,
        snapshot: Arc::new(Mutex::new(Snapshot::default())),
        subscribers: Arc::new(Mutex::new(Vec::new())),
        wake: Arc::new(AtomicBool::new(false)),
        stop: Arc::new(AtomicBool::new(false)),
    };
    let events = engine.survey(&mut model, now);
    engine.publish(&model, events);
    assert_eq!(model.rec.households.len(), 1);
    assert_eq!(model.rec.subscriptions.len(), 4);
    assert!(model.sink.as_ref().unwrap().local_addr().ip().is_loopback());
    while engine.next_event(&mut model, Duration::ZERO).is_some() {}
    model.rec.subscriptions.defer_network();
    (sim, engine, model, media)
}

fn wait_for(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !ready() {
        assert!(Instant::now() < deadline, "callback recovery did not converge");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn request(addr: SocketAddr, method: &str, path: &str, body: &str) -> Vec<u8> {
    let mut socket = TcpStream::connect_timeout(&addr, WAIT).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    write!(
        socket,
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
         SID: uuid:callback-overload-fixture\r\nSEQ: 0\r\nNT: upnp:event\r\n\
         NTS: upnp:propchange\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .unwrap();
    let mut response = Vec::new();
    socket.take(1024 * 1024).read_to_end(&mut response).unwrap();
    response
}

fn overload(addr: SocketAddr) {
    let body = "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\">\
                <e:property><Padding>test</Padding></e:property></e:propertyset>";
    for _ in 0..=EVENT_BACKLOG {
        let response = request(addr, "NOTIFY", "/RenderingControl", body);
        if response.starts_with(b"HTTP/1.1 503") {
            return;
        }
        assert!(response.starts_with(b"HTTP/1.1 200"));
    }
    panic!("the real callback inbox never reported overload");
}

fn media_still_served(addr: SocketAddr) {
    let response = request(addr, "GET", "/media/clip.wav", "");
    assert!(response.starts_with(b"HTTP/1.1 200"));
    assert!(response.ends_with(CLIP));
}

fn finish(sim: SimHandle, engine: &Engine, model: &mut Model) {
    engine.stop.store(true, Ordering::Release);
    progress::recover(engine, model);
    engine.release(model);
    sim.shutdown();
}

#[test]
fn real_overflow_preserves_media_and_repairs_a_quiet_lost_tail_on_s1_and_s2() {
    for generation in [1, 2] {
        let (sim, engine, mut model, _media) = fixture(generation);
        let addr = model.sink.as_ref().unwrap().local_addr();
        let player = &sim.players()[0];
        let id = PlayerId(player.uuid.clone());
        let host = player.ip;
        let before = model.playback.of(&id).unwrap().volume;
        let expected = if before == Some(47) { 48 } else { 47 };
        let plan = model.rec.inventory_plan(&engine.config.seeds);
        model.last_error = Some("unrelated inventory diagnostic".into());
        let (send, receive) = std::sync::mpsc::sync_channel(64);
        engine.subscribers.lock().unwrap().push(delivery::Pending::new(send));
        media_still_served(addr);
        overload(addr);
        let log_start = sim.gena_log().len();
        set_volume(&sim.lan(), host, expected).unwrap();
        wait_for(|| {
            sim.gena_log()[log_start..]
                .iter()
                .any(|entry| matches!(entry.event, GenaEvent::Delivered { status: 503, .. }))
        });
        let soap_calls = sim.soap_log().len();
        let mut resynced = false;
        for _ in 0..=EVENT_BACKLOG + 16 {
            let events = engine.next_event(&mut model, Duration::ZERO).unwrap();
            resynced = events.contains(&LiveEvent::Resync);
            engine.publish(&model, events);
            if resynced {
                break;
            }
        }
        assert!(resynced, "a dropped final event must surface without another callback");
        assert_eq!(model.sink.as_ref().unwrap().local_addr(), addr);
        assert_eq!(model.rec.subscriptions.len(), 4);
        assert!(!plan.current(&model.rec));
        assert!(model.rec.schedule.due(Instant::now()));
        assert_eq!(model.playback.of(&id).unwrap().volume, before);
        assert_eq!(model.last_error.as_deref(), Some("unrelated inventory diagnostic"));
        assert!(receive.try_iter().any(|event| event == LiveEvent::Resync));
        media_still_served(addr);
        // No new speaker mutation or synthetic delta is needed. Read recovery
        // uses the existing bounded sweep and does not reopen the listener.
        wait_for(|| {
            progress::recover(&engine, &mut model);
            model.playback.of(&id).unwrap().volume == Some(expected)
        });
        assert_eq!(model.sink.as_ref().unwrap().local_addr(), addr);
        assert!(sim.soap_log()[soap_calls..].iter().all(|call| call.action.starts_with("Get")));
        media_still_served(addr);
        finish(sim, &engine, &mut model);
    }
}

struct HeldRead {
    inner: Arc<dyn Transport + Send + Sync>,
    armed: AtomicBool,
    entered: AtomicBool,
    released: Arc<AtomicBool>,
}

impl Transport for HeldRead {
    fn soap_post(&self, host: IpAddr, path: &str, action: &str, body: &str) -> Result<String, ProtoError> {
        let result = self.inner.soap_post(host, path, action, body);
        if action.ends_with("#GetVolume") && self.armed.swap(false, Ordering::AcqRel) {
            self.entered.store(true, Ordering::Release);
            wait_for(|| self.released.load(Ordering::Acquire));
        }
        result
    }
}

struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[test]
fn a_pre_loss_read_cannot_roll_state_back_after_recovery_was_requested() {
    let (sim, mut engine, mut model, _media) = fixture(2);
    let host = sim.players()[0].ip;
    let id = PlayerId(sim.players()[0].uuid.clone());
    let _ = model.playback.apply_room_levels(&id, 31, false);
    set_volume(&sim.lan(), host, 23).unwrap();
    let released = Arc::new(AtomicBool::new(false));
    let _release = Release(Arc::clone(&released));
    let held = Arc::new(HeldRead {
        inner: Arc::clone(&engine.transport),
        armed: AtomicBool::new(true),
        entered: AtomicBool::new(false),
        released: Arc::clone(&released),
    });
    engine.transport = held.clone();
    progress::request_survey(&mut model);
    progress::recover(&engine, &mut model);
    wait_for(|| held.entered.load(Ordering::Acquire));
    set_volume(&sim.lan(), host, 47).unwrap();
    let calls = sim.soap_log().len();
    assert_eq!(engine.callback_loss(&mut model), vec![LiveEvent::Resync]);
    assert_eq!(model.playback.of(&id).unwrap().volume, Some(31));
    released.store(true, Ordering::Release);
    wait_for(|| {
        progress::recover(&engine, &mut model);
        let volume = model.playback.of(&id).unwrap().volume;
        assert_ne!(volume, Some(23), "a cancelled pre-loss sample was adopted");
        volume == Some(47)
    });
    assert!(sim.soap_log()[calls..].iter().all(|call| call.action.starts_with("Get")));
    let next = model.rec.schedule.next_at();
    engine.stop.store(true, Ordering::Release);
    assert!(engine.callback_loss(&mut model).is_empty());
    assert_eq!(model.rec.schedule.next_at(), next, "shutdown cannot schedule recovery");
    finish(sim, &engine, &mut model);
}
