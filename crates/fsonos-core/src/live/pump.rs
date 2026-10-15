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
    // Buffered events and subscription maintenance take precedence over
    // suspected track-end loss. Recovery is at most one read-back per turn.
    super::progress::recover(engine, model);
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
    use fsonos_sim::{GenaEvent, NotifyDrop, SimHandle, SimHousehold, SimModel, SimPlayerSpec};
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

    fn buffered(generation: u8, drop_initial: bool) -> (SimHandle, Engine, Model, PlayerId) {
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
        let late = INITIAL_STATE_WAIT + Duration::from_secs(1);
        let started = Instant::now().checked_sub(late).unwrap();
        let mut rec = Reconciler::new(Duration::from_secs(300), Duration::from_secs(300), started);
        rec.households.push(house);
        rec.schedule.succeeded(Instant::now());
        let wants = wanted(&rec.households);
        assert_eq!(wants.len(), 4);
        let id = wants[0].player.clone();
        let local = lan.local_address_toward(host).unwrap();
        let sink = EventSink::start(SocketAddr::new(local, 0)).unwrap();
        let callback = |service: Service| sink.callback_url(service.tag());
        if drop_initial {
            sim.drop_notifies("Room", Some(NotifyDrop::Next(4)))
                .unwrap();
        }
        let report = rec.subscriptions.sync(&*lan, &wants, callback, started);
        assert_eq!(report.subscribed, wants.len(),);
        assert!(report.failed.is_empty());
        // Wait for the complete initial burst to be delivered or dropped.
        // The live worker has not consumed anything from the listener yet.
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let completed = sim
                .gena_log()
                .iter()
                .filter(|e| match e.event {
                    GenaEvent::Dropped { seq: 0, .. } => drop_initial,
                    GenaEvent::Delivered {
                        seq: 0,
                        status: 200,
                        ..
                    } => !drop_initial,
                    _ => false,
                })
                .count();
            if completed == wants.len() {
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
            remembered: Vec::new(),
            rec,
            playback: Playback::default(),
            progress: super::super::progress::Watchdog::default(),
            sink: Some(sink),
            surveyed_at: Some(started),
            last_error: None,
            route_checked: started,
        };
        (sim, engine, model, id)
    }

    #[test]
    fn buffered_initials_are_applied_before_expired_watchdogs_on_both_generations() {
        for generation in [1, 2] {
            let (sim, engine, mut model, id) = buffered(generation, false);
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
        let (sim, engine, mut model, id) = buffered(2, false);
        engine.stop.store(true, Ordering::Release);
        let before = sim.gena_log().len();
        turn(&engine, &mut model);
        assert!(model.playback.of(&id).is_none());
        assert_eq!(sim.gena_log().len(), before);
        engine.release(&mut model);
        sim.shutdown();
    }

    #[test]
    fn genuine_silence_still_triggers_live_worker_recovery_on_both_generations() {
        for generation in [1, 2] {
            let (sim, engine, mut model, id) = buffered(generation, true);
            let calls = sim.soap_log().len();
            let until = Instant::now() + Duration::from_secs(5);
            while model
                .rec
                .subscriptions
                .next_due()
                .is_some_and(|due| due <= Instant::now() + INITIAL_STATE_WAIT * 2)
            {
                assert!(Instant::now() < until, "live worker did not recover state");
                turn(&engine, &mut model);
            }
            let state = model.playback.of(&id).unwrap();
            assert_eq!(state.transport, Some(TransportState::Stopped));
            assert_eq!(state.volume, Some(23));
            assert_eq!(state.mute, Some(true));
            assert!(state.group_volume.is_some());
            let subscribed = sim
                .gena_log()
                .iter()
                .filter(|e| matches!(e.event, GenaEvent::Subscribed { .. }))
                .count();
            assert_eq!(subscribed, 8, "each silent service replaced once");
            assert_eq!(sim.soap_log().len(), calls);
            engine.release(&mut model);
            sim.shutdown();
        }
    }

    #[test]
    fn upstream_overflow_flushes_against_the_committed_snapshot_without_network_io() {
        use crate::live::LiveEvent;
        use crate::playback::EventSource;
        use fsonos_proto::gena::Notify;
        use std::sync::mpsc::{TryRecvError, sync_channel};

        for generation in [1, 2] {
            let (sim, engine, mut model, id) = buffered(generation, false);
            turn(&engine, &mut model);
            let (send, receive) = sync_channel(1);
            engine
                .subscribers
                .lock()
                .unwrap()
                .push(super::super::delivery::Pending::new(send));
            let before = sim.soap_log().len();
            // Real parsed playback deltas, published by the production Engine.
            // Its deliberately tiny subscriber stops consuming for a burst.
            for volume in [24, 25, 26] {
                let event = Notify {
                    sid: "uuid:test".into(),
                    seq: volume,
                    path: "/RenderingControl".into(),
                    properties: vec![(
                        "LastChange".into(),
                        format!(
                            "<Event><InstanceID val=\"0\"><Volume channel=\"Master\" val=\"{volume}\"/></InstanceID></Event>"
                        ),
                    )],
                };
                let changes = model
                    .playback
                    .apply(&id, EventSource::RenderingControl, &event, Instant::now())
                    .unwrap();
                engine.publish(
                    &model,
                    vec![LiveEvent::Playback {
                        player: id.clone(),
                        changes,
                    }],
                );
            }
            assert!(matches!(receive.try_recv(), Ok(LiveEvent::Playback { .. })));
            // This is also called on quiet outer-loop turns, not just on NOTIFY.
            engine.emit(&[]);
            assert_eq!(receive.try_recv(), Ok(LiveEvent::Resync));
            assert_eq!(
                engine
                    .snapshot
                    .lock()
                    .unwrap()
                    .playback
                    .of(&id)
                    .unwrap()
                    .volume,
                Some(26)
            );
            engine.emit(&[]);
            assert_eq!(receive.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(sim.soap_log().len(), before);
            drop(receive);
            engine.emit(&[LiveEvent::Topology]);
            assert!(engine.subscribers.lock().unwrap().is_empty());
            engine.release(&mut model);
            sim.shutdown();
        }
    }

    #[test]
    fn missed_duplicate_track_and_pause_events_are_recovered_without_playback_writes() {
        use fsonos_proto::control::{
            add_uri_to_queue, next, pause, play, play_from_queue, seek_track,
        };
        use std::sync::mpsc::sync_channel;

        for generation in [1, 2] {
            let (sim, engine, mut model, id) = buffered(generation, false);
            turn(&engine, &mut model);
            let host = sim.player("Room").unwrap().ip;
            // The callback exists, but all following NOTIFYs are deliberately
            // dropped. Recovery must use the real SOAP endpoints instead.
            sim.drop_notifies("Room", Some(NotifyDrop::Next(100)))
                .unwrap();
            for _ in 0..2 {
                add_uri_to_queue(&*engine.lan, host, "test:duplicate", "", false).unwrap();
            }
            play_from_queue(&*engine.lan, host, &id).unwrap();
            seek_track(&*engine.lan, host, 1).unwrap();
            play(&*engine.lan, host).unwrap();
            let (mut sample, _) = super::super::observation::read(&engine.via(), host, || false)
                .unwrap()
                .unwrap();
            assert_eq!(sample.position.track, 1);
            // Move only the cached deadline into the past. No wall-clock
            // sleeps through track durations are needed for this regression.
            sample.position.duration_secs = Some(1);
            sample.position.position_secs = Some(0);
            let overdue = Instant::now().checked_sub(Duration::from_secs(7)).unwrap();
            model.playback.apply_observation(&id, &sample, overdue);
            engine.publish(&model, Vec::new());
            let (send, receive) = sync_channel(16);
            engine
                .subscribers
                .lock()
                .unwrap()
                .push(super::super::delivery::Pending::new(send));

            next(&*engine.lan, host).unwrap();
            let before = sim.soap_log().len();
            turn(&engine, &mut model);
            assert_eq!(model.playback.of(&id).unwrap().queue_position, Some(2));
            let event = receive.recv_timeout(Duration::from_secs(1)).unwrap();
            assert!(
                matches!(event, super::super::LiveEvent::Playback { changes, .. }
                if changes.track == Some((Some("test:duplicate".into()), Some("test:duplicate".into()))))
            );
            let log = sim.soap_log();
            assert_eq!(
                log[before..]
                    .iter()
                    .map(|entry| entry.action.as_str())
                    .collect::<Vec<_>>(),
                [
                    "GetTransportInfo",
                    "GetMediaInfo",
                    "GetPositionInfo",
                    "GetMediaInfo",
                    "GetTransportInfo"
                ]
            );
            assert_eq!(
                engine
                    .snapshot
                    .lock()
                    .unwrap()
                    .playback
                    .of(&id)
                    .unwrap()
                    .queue_position,
                Some(2)
            );

            let (mut sample, _) = super::super::observation::read(&engine.via(), host, || false)
                .unwrap()
                .unwrap();
            sample.position.duration_secs = Some(1);
            sample.position.position_secs = Some(0);
            model.playback.apply_observation(&id, &sample, overdue);
            pause(&*engine.lan, host).unwrap();
            let before = sim.soap_log().len();
            turn(&engine, &mut model);
            let state = model.playback.of(&id).unwrap();
            assert_eq!(state.transport, Some(TransportState::Paused));
            assert_eq!(state.volume, Some(23));
            assert_eq!(state.mute, Some(true));
            let log = sim.soap_log();
            assert_eq!(log.len() - before, 5);
            assert!(
                log[before..]
                    .iter()
                    .all(|entry| entry.action.starts_with("Get"))
            );
            let before = log.len();
            turn(&engine, &mut model);
            assert_eq!(
                sim.soap_log().len(),
                before,
                "paused playback is not polled or resumed"
            );
            engine.release(&mut model);
            sim.shutdown();
        }
    }
}
