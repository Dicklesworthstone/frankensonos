//! Consume buffered notifications before deciding that a stream is silent.
//! A slow survey can outlast the initial-state deadline while its NOTIFYs
//! wait in the callback queue. A bounded burst avoids mistaking that backlog
//! for packet loss without starving renewals, surveys, or shutdown.

use super::{Engine, Model, TICK};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

const EVENT_BURST: usize = 256;

pub(super) fn turn(engine: &Engine, model: &mut Model) {
    drain(|| {
        if engine.stop.load(Ordering::Acquire) {
            return false;
        }
        let Some(pushed) = engine.next_event(model, Duration::ZERO) else {
            return false;
        };
        engine.publish(model, pushed);
        true
    });
    if engine.stop.load(Ordering::Acquire) {
        return;
    }
    // Refresh time after potentially slow survey/event processing, not from
    // the beginning of the outer loop. Accepted initial events disarm their
    // watchdogs before this pass chooses which subscriptions need recovery.
    let now = Instant::now();
    if model
        .rec
        .subscriptions
        .next_due()
        .is_some_and(|due| due <= now)
    {
        engine.renew(model, now);
        engine.publish(model, Vec::new());
    }
    if !engine.stop.load(Ordering::Acquire)
        && let Some(pushed) = engine.next_event(model, TICK)
    {
        engine.publish(model, pushed);
    }
}

fn drain(mut next: impl FnMut() -> bool) {
    for _ in 0..EVENT_BURST {
        if !next() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::HouseholdState;
    use crate::events::{INITIAL_STATE_WAIT, Service, wanted};
    use crate::live::{LiveConfig, Snapshot};
    use crate::playback::Playback;
    use crate::reconcile::Reconciler;
    use fsonos_proto::control::{set_mute, set_volume};
    use fsonos_proto::net::{EventSink, Lan};
    use fsonos_proto::topology::get_zone_group_state;
    use fsonos_sim::{GenaEvent, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_types::{PlayerId, TransportState};
    use std::net::SocketAddr;
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};
    use std::time::SystemTime;

    #[test]
    fn continuous_intake_yields_after_a_bounded_burst() {
        let mut taken = 0;
        drain(|| {
            taken += 1;
            true
        });
        assert_eq!(taken, EVENT_BURST);
    }

    #[test]
    fn empty_or_stopped_intake_does_not_spin() {
        for available in [0, 3] {
            let mut attempts = 0;
            drain(|| {
                attempts += 1;
                attempts <= available
            });
            assert_eq!(attempts, available + 1);
        }
    }

    fn buffered(generation: u8) -> (SimHandle, Engine, Model, PlayerId) {
        let builder = SimHousehold::builder();
        let sim = match generation {
            1 => builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)]),
            _ => builder.s2([SimPlayerSpec::new("Room", SimModel::One)]),
        }
        .spawn()
        .unwrap();
        let host = sim.player("Room").unwrap().ip;
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        set_volume(&*lan, host, 23).unwrap();
        set_mute(&*lan, host, true).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&*lan, host).unwrap());
        // Represent a survey whose network work took longer than the initial
        // deadline. No test sleeps through that interval.
        let started = Instant::now() - INITIAL_STATE_WAIT - Duration::from_secs(1);
        let mut rec = Reconciler::new(
            Duration::from_secs(300),
            Duration::from_secs(300),
            started,
        );
        rec.households.push(house);
        rec.schedule.succeeded(Instant::now());
        let wants = wanted(&rec.households);
        assert_eq!(wants.len(), 4);
        let id = wants[0].player.clone();
        let local = lan.local_address_toward(host).unwrap();
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let callback = |service: Service| sink.callback_url(service.tag());
        let report = rec.subscriptions.sync(&*lan, &wants, callback, started);
        assert_eq!(report.subscribed, wants.len());
        assert!(report.failed.is_empty());
        // Delivery acknowledgements prove the HTTP listener has enqueued
        // every initial event, but the live worker has not consumed any.
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let delivered = sim
                .gena_log()
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
                .count();
            if delivered == wants.len() {
                break;
            }
            assert!(Instant::now() < until, "initial delivery did not complete");
            std::thread::sleep(Duration::from_millis(10));
        }
        let engine = Engine {
            transport: Arc::new(sim.lan()),
            lan,
            config: LiveConfig::new(vec![host]),
            snapshot: Arc::new(Mutex::new(Snapshot::default())),
            subscribers: Arc::new(Mutex::new(Vec::new())),
            wake: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        };
        let model = Model {
            ticked: (started, SystemTime::now()),
            rec,
            playback: Playback::default(),
            sink: Some(sink),
            surveyed_at: Some(started),
            last_error: None,
        };
        (sim, engine, model, id)
    }

    #[test]
    fn buffered_initials_are_applied_before_expired_watchdogs_on_both_generations() {
        for generation in [1, 2] {
            let (sim, engine, mut model, id) = buffered(generation);
            assert!(model.rec.subscriptions.next_due().unwrap() < Instant::now());
            let calls = sim.soap_log().len();
            turn(&engine, &mut model);
            let state = model.playback.of(&id).unwrap();
            assert_eq!(state.transport, Some(TransportState::Stopped));
            assert_eq!(state.volume, Some(23));
            assert_eq!(state.mute, Some(true));
            assert!(model.rec.subscriptions.next_due().unwrap() > Instant::now());
            let subscribed = sim
                .gena_log()
                .iter()
                .filter(|e| matches!(e.event, GenaEvent::Subscribed { .. }))
                .count();
            assert_eq!(subscribed, 4, "buffered events were not lost");
            assert_eq!(sim.soap_log().len(), calls);
            let published = engine.snapshot.lock().unwrap();
            assert_eq!(published.playback.of(&id), Some(state));
            drop(published);
            engine.release(&mut model);
            sim.shutdown();
        }
    }

    #[test]
    fn shutdown_requested_before_a_turn_neither_consumes_nor_resubscribes() {
        let (sim, engine, mut model, id) = buffered(2);
        engine.stop.store(true, Ordering::Release);
        let before = sim.gena_log().len();
        turn(&engine, &mut model);
        assert!(model.playback.of(&id).is_none());
        assert_eq!(sim.gena_log().len(), before);
        engine.release(&mut model);
        sim.shutdown();
    }
}
