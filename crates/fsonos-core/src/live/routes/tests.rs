use super::*;
use crate::live::{Engine, LiveConfig, Model, ROUTE_SETTLE, Snapshot, progress};
use crate::playback::Playback;
use crate::reconcile::{Health, Reconciler};
use fsonos_proto::control::set_volume;
use fsonos_proto::net::MediaFiles;
use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, Ipv6Addr, TcpStream};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

const WAIT: Duration = Duration::from_secs(10);
const BYTES: &[u8] = b"route-specific synthetic clip";

fn host(index: u8) -> IpAddr {
    Ipv4Addr::new(192, 0, 2, index).into()
}

fn local(index: u8) -> IpAddr {
    if index == 1 {
        Ipv4Addr::LOCALHOST.into()
    } else {
        Ipv6Addr::LOCALHOST.into()
    }
}

fn open(address: IpAddr) -> Result<EventSink, String> {
    EventSink::start(SocketAddr::new(address, 0)).map_err(|error| error.to_string())
}

fn pair() -> Listeners {
    let mut listeners = Listeners::default();
    let changed = listeners.reconcile(
        &[host(1), host(2)],
        Instant::now(),
        false,
        |toward| {
            Ok(if toward == host(1) {
                local(1)
            } else {
                local(2)
            })
        },
        open,
    );
    assert_eq!(changed, [host(1), host(2)]);
    assert!(listeners.diagnostic().is_none());
    assert_eq!(listeners.listening.len(), 2);
    listeners
}

fn address(base: &str) -> SocketAddr {
    base.strip_prefix("http://").unwrap().parse().unwrap()
}

fn request(addr: SocketAddr, method: &str, path: &str, sid: &str, host: &str) -> Vec<u8> {
    let body = "<e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><Volume>31</Volume></e:property></e:propertyset>";
    let mut socket = TcpStream::connect_timeout(&addr, WAIT).unwrap();
    socket.set_read_timeout(Some(WAIT)).unwrap();
    socket.set_write_timeout(Some(WAIT)).unwrap();
    write!(socket, "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\nSID: {sid}\r\nSEQ: 0\r\nNT: upnp:event\r\nNTS: upnp:propchange\r\nContent-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    let mut response = Vec::new();
    socket.take(1024 * 1024).read_to_end(&mut response).unwrap();
    response
}

fn send(base: &str, sid: &str) {
    let addr = address(base);
    let response = request(addr, "NOTIFY", "/RenderingControl", sid, &addr.to_string());
    assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
}

fn media(base: &str) {
    let addr = address(base);
    let response = request(addr, "GET", "/media/clip.wav", "unused", &addr.to_string());
    assert!(response.starts_with(b"HTTP/1.1 200"), "{response:?}");
    assert!(response.ends_with(BYTES));
}

#[test]
fn known_players_get_their_own_specific_callback_and_foreign_hosts_are_refused() {
    let listeners = pair();
    assert_eq!(
        address(&listeners.base_for(host(1)).unwrap()).ip(),
        local(1)
    );
    assert_eq!(
        address(&listeners.base_for(host(2)).unwrap()).ip(),
        local(2)
    );
    assert_eq!(listeners.base_for(host(3)), None);
    for index in [1, 2] {
        let base = listeners.base_for(host(index)).unwrap();
        let reply = request(
            address(&base),
            "NOTIFY",
            "/RenderingControl",
            "uuid:foreign",
            "unrelated.invalid",
        );
        assert!(reply.starts_with(b"HTTP/1.1 421"));
    }
    assert!(listeners.recv_event(Duration::ZERO).unwrap().is_none());
}

#[test]
fn a_hot_interface_cannot_starve_the_other_listener() {
    let listeners = pair();
    let first = listeners.base_for(host(1)).unwrap();
    let second = listeners.base_for(host(2)).unwrap();
    for _ in 0..4 {
        send(&first, "uuid:busy");
    }
    send(&second, "uuid:quiet");
    let mut heard = Vec::new();
    for _ in 0..2 {
        let Some(CallbackEvent::Notify(notify)) = listeners.recv_event(WAIT).unwrap() else {
            panic!("expected an actual callback");
        };
        heard.push(notify.sid);
    }
    assert!(heard.iter().any(|sid| sid == "uuid:quiet"));
    assert!(heard.iter().any(|sid| sid == "uuid:busy"));
}

#[test]
fn one_failed_bind_keeps_the_other_callback_and_retries_only_when_due() {
    let now = Instant::now();
    let mut listeners = Listeners::default();
    let resolve = |toward| {
        Ok(if toward == host(1) {
            local(1)
        } else {
            local(2)
        })
    };
    let mut attempts = 0;
    let mut fail = |address| {
        attempts += 1;
        if address == local(2) {
            Err("cannot listen on the second interface".into())
        } else {
            open(address)
        }
    };
    assert_eq!(
        listeners.reconcile(&[host(1), host(2)], now, false, resolve, &mut fail),
        [host(1)]
    );
    let primary = listeners.base_for(host(1)).unwrap();
    assert_eq!(listeners.base_for(host(2)), None);
    assert!(listeners.diagnostic().unwrap().contains("second interface"));
    assert!(
        listeners
            .reconcile(
                &[host(1), host(2)],
                now + Duration::from_secs(1),
                false,
                resolve,
                &mut fail
            )
            .is_empty()
    );
    assert_eq!(attempts, 2, "a failed bind keeps its cooldown");
    assert_eq!(
        listeners.reconcile(
            &[host(1), host(2)],
            now + EVENTLESS_RETRY,
            false,
            resolve,
            open
        ),
        [host(2)]
    );
    assert_eq!(listeners.base_for(host(1)).as_ref(), Some(&primary));
    assert!(listeners.base_for(host(2)).is_some());
    assert!(listeners.diagnostic().is_none());
}

#[test]
fn lost_route_resolution_retires_new_admission_but_keeps_media_for_existing_clips() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("clip.wav");
    std::fs::write(&path, BYTES).unwrap();
    let files: MediaFiles = Arc::new(move |name| (name == "clip.wav").then(|| path.clone()));
    let now = Instant::now();
    let mut listeners = Listeners::default();
    let resolve = |toward| {
        Ok(if toward == host(1) {
            local(1)
        } else {
            local(2)
        })
    };
    listeners.reconcile(&[host(1), host(2)], now, false, resolve, |address| {
        EventSink::start_serving(SocketAddr::new(address, 0), Some(Arc::clone(&files)))
            .map_err(|error| error.to_string())
    });
    let primary = listeners.base_for(host(1)).unwrap();
    let previous = listeners.base_for(host(2)).unwrap();
    let changed = listeners.reconcile(
        &[host(1), host(2)],
        now + Duration::from_secs(1),
        false,
        |toward| {
            if toward == host(2) {
                Err("no current route toward second household".into())
            } else {
                Ok(local(1))
            }
        },
        |_| panic!("working sockets must stay open"),
    );
    assert_eq!(changed, [host(2)]);
    assert_eq!(listeners.base_for(host(2)), None);
    assert_eq!(listeners.base_for(host(1)).as_ref(), Some(&primary));
    media(&previous);
    assert!(listeners.diagnostic().unwrap().contains("no current route"));
    assert_eq!(
        listeners.reconcile(
            &[host(1), host(2)],
            now + Duration::from_secs(2),
            false,
            resolve,
            |_| panic!("the original listener remains usable")
        ),
        [host(2)]
    );
    assert_eq!(listeners.base_for(host(2)).as_ref(), Some(&previous));
    assert!(listeners.diagnostic().is_none());
}

#[test]
fn a_route_move_settles_then_changes_only_that_players_callback() {
    let now = Instant::now();
    let mut listeners = Listeners::default();
    listeners.reconcile(&[host(1), host(2)], now, false, |_| Ok(local(1)), open);
    let first = listeners.base_for(host(1)).unwrap();
    assert_eq!(listeners.base_for(host(2)).as_ref(), Some(&first));
    let resolve = |toward| {
        Ok(if toward == host(1) {
            local(1)
        } else {
            local(2)
        })
    };
    assert!(
        listeners
            .reconcile(&[host(1), host(2)], now, false, resolve, open)
            .is_empty()
    );
    assert!(
        listeners
            .reconcile(
                &[host(1), host(2)],
                now + ROUTE_SETTLE - Duration::from_secs(1),
                false,
                resolve,
                open
            )
            .is_empty()
    );
    assert_eq!(
        listeners.reconcile(
            &[host(1), host(2)],
            now + ROUTE_SETTLE,
            false,
            resolve,
            open
        ),
        [host(2)]
    );
    assert_eq!(listeners.base_for(host(1)).as_ref(), Some(&first));
    assert_eq!(
        address(&listeners.base_for(host(2)).unwrap()).ip(),
        local(2)
    );
    assert_eq!(listeners.listening.len(), 2);
}

#[test]
fn closing_one_listener_keeps_the_others_url_and_obeys_rebind_backoff() {
    let mut listeners = pair();
    let now = Instant::now();
    let first = listeners.base_for(host(1)).unwrap();
    let closed = Closed {
        local: local(2),
        error: ProtoError::NotWired("second listener stopped"),
    };
    assert_eq!(listeners.closed(&closed, now), [host(2)]);
    assert_eq!(listeners.base_for(host(2)), None);
    assert_eq!(listeners.base_for(host(1)).as_ref(), Some(&first));
    let resolve = |toward| {
        Ok(if toward == host(1) {
            local(1)
        } else {
            local(2)
        })
    };
    listeners.reconcile(&[host(1), host(2)], now, false, resolve, |_| {
        panic!("bind must back off")
    });
    assert_eq!(
        listeners.reconcile(
            &[host(1), host(2)],
            now + EVENTLESS_RETRY,
            false,
            resolve,
            open
        ),
        [host(2)]
    );
    assert_eq!(listeners.base_for(host(1)).as_ref(), Some(&first));
    assert!(listeners.diagnostic().is_none());
}

#[test]
fn unresolved_routes_eventually_retire_their_last_media_socket() {
    let now = Instant::now();
    let mut listeners = Listeners::default();
    listeners.reconcile(&[host(1)], now, false, |_| Ok(local(1)), open);
    let old = address(&listeners.base_for(host(1)).unwrap());
    let unavailable = |_| Err("local route is unavailable".into());
    assert_eq!(
        listeners.reconcile(&[host(1)], now, false, unavailable, open),
        [host(1)]
    );
    assert_eq!(listeners.base_for(host(1)), None);
    assert!(
        !listeners.is_empty(),
        "already admitted media keeps its listener"
    );
    listeners.reconcile(&[host(1)], now + MEDIA_RETIRE, false, unavailable, open);
    assert!(listeners.is_empty());
    assert!(listeners.diagnostic().is_some());
    assert!(TcpStream::connect_timeout(&old, Duration::from_secs(1)).is_err());
    assert!(listeners.recv_event(Duration::ZERO).unwrap().is_none());
}

fn fixture() -> (SimHandle, Engine, Model) {
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
    let config = LiveConfig::new(sim.players().iter().map(|p| p.ip).collect());
    let now = Instant::now();
    let mut listeners = Listeners::default();
    let legacy = sim.player("Legacy").unwrap().ip;
    listeners.reconcile(
        &config.seeds,
        now,
        false,
        |toward| Ok(if toward == legacy { local(1) } else { local(2) }),
        open,
    );
    let mut model = Model {
        ticked: (now, SystemTime::now()),
        remembered: Vec::new(),
        rec: Reconciler::new(config.interval, config.max_backoff, now),
        playback: Playback::default(),
        progress: progress::Watchdog::default(),
        sink: Some(listeners),
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
    (sim, engine, model)
}

fn finish(sim: SimHandle, engine: &Engine, model: &mut Model) {
    engine.stop.store(true, Ordering::Release);
    engine.release(model);
    sim.shutdown();
}

#[test]
fn s1_and_s2_subscriptions_deliver_on_their_individual_callback_routes() {
    let (sim, engine, mut model) = fixture();
    assert_eq!(model.rec.subscriptions.len(), 10);
    for (name, index) in [("Legacy", 1), ("Modern", 2)] {
        let player = sim.player(name).unwrap();
        let base = model.sink.as_ref().unwrap().base_for(player.ip).unwrap();
        assert_eq!(address(&base).ip(), local(index));
        assert_eq!(
            engine
                .snapshot
                .lock()
                .unwrap()
                .callbacks
                .get(&PlayerId(player.uuid.clone())),
            Some(&base)
        );
        let callbacks: Vec<_> = sim
            .gena_log()
            .into_iter()
            .filter_map(|entry| {
                if entry.player != player.uuid {
                    return None;
                }
                match entry.event {
                    GenaEvent::Subscribed { callback, .. } => Some(callback),
                    _ => None,
                }
            })
            .collect();
        assert_eq!(callbacks.len(), 5);
        assert!(
            callbacks
                .iter()
                .all(|callback| callback.starts_with(&format!("{base}/")))
        );
    }
    let until = Instant::now() + WAIT;
    while model
        .playback
        .of(&PlayerId(sim.player("Modern").unwrap().uuid.clone()))
        .is_none()
    {
        assert!(Instant::now() < until);
        engine.next_event(&mut model, Duration::from_millis(20));
    }
    for (name, level) in [("Legacy", 43), ("Modern", 57)] {
        set_volume(&sim.lan(), sim.player(name).unwrap().ip, level).unwrap();
    }
    let calls = sim.soap_log().len();
    loop {
        let complete = [("Legacy", 43), ("Modern", 57)]
            .into_iter()
            .all(|(name, expected)| {
                let id = PlayerId(sim.player(name).unwrap().uuid.clone());
                model
                    .playback
                    .of(&id)
                    .is_some_and(|state| state.volume == Some(expected))
            });
        if complete {
            break;
        }
        assert!(
            Instant::now() < until,
            "both callback interfaces must deliver"
        );
        engine.next_event(&mut model, Duration::from_millis(20));
    }
    assert_eq!(
        sim.soap_log().len(),
        calls,
        "GENA updates need no SOAP polling"
    );
    finish(sim, &engine, &mut model);
}

#[test]
fn local_listener_loss_does_not_mark_its_reachable_household_offline() {
    let (sim, engine, mut model) = fixture();
    let first = model
        .sink
        .as_ref()
        .unwrap()
        .base_for(sim.player("Legacy").unwrap().ip)
        .unwrap();
    engine.listener_failed(
        &mut model,
        Closed {
            local: local(2),
            error: ProtoError::NotWired("second callback listener stopped"),
        },
    );
    assert_eq!(model.rec.subscriptions.len(), 5);
    let snapshot = engine.snapshot.lock().unwrap().clone();
    for name in ["Legacy", "Modern"] {
        assert_eq!(
            snapshot
                .health
                .of(&PlayerId(sim.player(name).unwrap().uuid.clone()))
                .unwrap()
                .health,
            Health::Healthy
        );
    }
    assert_eq!(
        snapshot
            .callbacks
            .get(&PlayerId(sim.player("Legacy").unwrap().uuid.clone())),
        Some(&first)
    );
    assert!(
        !snapshot
            .callbacks
            .contains_key(&PlayerId(sim.player("Modern").unwrap().uuid.clone()))
    );
    assert!(snapshot.last_error.unwrap().contains("second callback"));
    assert!(model.rec.schedule.next_at() <= Instant::now() + EVENTLESS_RETRY);
    finish(sim, &engine, &mut model);
}

#[test]
fn the_live_model_publishes_no_callback_after_its_last_retired_socket_expires() {
    let (sim, mut engine, mut model) = fixture();
    engine.config.seeds.clear();
    model.rec.households.clear();
    let now = Instant::now();
    let _ = engine.reconcile_routes(&mut model, now, false);
    assert!(
        model.sink.is_some(),
        "old media URLs retain their grace period"
    );
    let _ = engine.reconcile_routes(&mut model, now + MEDIA_RETIRE, false);
    assert!(model.sink.is_none());
    let snapshot = engine.snapshot.lock().unwrap().clone();
    assert!(snapshot.callback.is_none());
    assert!(snapshot.callbacks.is_empty());
    finish(sim, &engine, &mut model);
}
