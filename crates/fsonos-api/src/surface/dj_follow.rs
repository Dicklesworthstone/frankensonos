//! Drive the DJ from live events, with bounded maintenance for quiet queues.

use super::Surface;
use fsonos_core::live::LiveEvent;
use std::collections::HashSet;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

/// Retry from cached state even if the speaker sends no further delta.
/// This is not a speaker poll: only an active feed that needs work does I/O.
const MAINTENANCE: Duration = Duration::from_secs(30);

pub(super) fn follow(surface: &Arc<Surface>) {
    let (Some(live), true) = (surface.live(), surface.dj.is_some()) else {
        return;
    };
    let changes = live.subscribe();
    let surface = Arc::downgrade(surface);
    if let Err(error) = std::thread::Builder::new()
        .name("fsonos-dj".into())
        .spawn(move || drive(&changes, MAINTENANCE, |event| handle(&surface, event)))
    {
        tracing::error!(%error, "could not start the DJ queue follower");
    }
}

fn handle(surface: &Weak<Surface>, event: Option<LiveEvent>) -> bool {
    let Some(surface) = surface.upgrade() else {
        return false;
    };
    let Some(live) = surface.live() else {
        return false;
    };
    match event {
        Some(LiveEvent::Playback { player, .. }) => surface.dj_playback(&player),
        Some(_) => {}
        None => {
            // A restored feed may have missed the initial events before
            // follow started. Use the latest model, never a saved event or
            // a direct household survey.
            let households = live.households();
            let mut seen = HashSet::new();
            for coordinator in households
                .iter()
                .flat_map(|h| h.groups.iter().map(|g| &g.coordinator))
            {
                if seen.insert(coordinator.clone()) {
                    surface.dj_playback(coordinator);
                }
            }
        }
    }
    true
}

/// `None` asks for maintenance; `Some` delivers an event. Returning false
/// stops the worker. Run once immediately, then at most once per interval,
/// including while a busy event stream never lets recv_timeout time out.
fn drive<T>(events: &Receiver<T>, interval: Duration, mut handle: impl FnMut(Option<T>) -> bool) {
    if !handle(None) {
        return;
    }
    let mut due = Instant::now() + interval;
    loop {
        match events.recv_timeout(due.saturating_duration_since(Instant::now())) {
            Ok(event) => {
                if !handle(Some(event)) {
                    return;
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        if Instant::now() >= due {
            if !handle(None) {
                return;
            }
            // Do not replay missed ticks after a slow network operation or
            // host sleep; one pass over current state is sufficient.
            due = Instant::now() + interval;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn starts_from_cached_state_without_waiting_for_an_event() {
        let (_send, events) = mpsc::channel::<()>();
        let mut calls = 0;
        drive(&events, Duration::from_secs(60), |event| {
            assert!(event.is_none());
            calls += 1;
            false
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn delivers_events_in_order_and_exits_when_the_model_stops() {
        let (send, events) = mpsc::channel();
        send.send(1).unwrap();
        send.send(2).unwrap();
        drop(send);
        let mut delivered = Vec::new();
        drive(&events, Duration::from_secs(60), |event| {
            delivered.push(event);
            true
        });
        assert_eq!(delivered, [None, Some(1), Some(2)]);
    }

    #[test]
    fn an_idle_stream_keeps_retrying_without_new_events() {
        let (_send, events) = mpsc::channel::<()>();
        let (observed, receive) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut calls = 0;
            drive(&events, Duration::from_millis(5), |event| {
                assert!(event.is_none());
                calls += 1;
                observed.send(calls).unwrap();
                calls < 3
            });
        });
        for expected in 1..=3 {
            assert_eq!(
                receive.recv_timeout(Duration::from_secs(2)).unwrap(),
                expected
            );
        }
        worker.join().unwrap();
    }

    #[test]
    fn unrelated_event_traffic_cannot_starve_maintenance() {
        let (send, events) = mpsc::channel();
        for n in 0..1000 {
            send.send(n).unwrap();
        }
        let mut events_seen = 0;
        let mut maintenance = 0;
        drive(&events, Duration::from_millis(5), |event| {
            if event.is_some() {
                events_seen += 1;
                std::thread::sleep(Duration::from_millis(1));
            } else {
                maintenance += 1;
            }
            maintenance < 2
        });
        assert_eq!(maintenance, 2);
        assert!(events_seen > 0 && events_seen < 1000);
    }

    #[test]
    fn dropping_the_surface_can_stop_the_worker_on_an_event() {
        let (send, events) = mpsc::channel();
        send.send(1).unwrap();
        send.send(2).unwrap();
        let mut delivered = Vec::new();
        drive(&events, Duration::from_secs(60), |event| {
            delivered.push(event);
            event.is_none()
        });
        assert_eq!(delivered, [None, Some(1)]);
    }
}

#[cfg(test)]
mod simulator_tests {
    use super::*;
    use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
    use crate::plan::DjAction;
    use crate::{Failure, OutcomeDto};
    use fsonos_core::clock::{Clock, SystemClock};
    use fsonos_core::live::{Live, LiveConfig};
    use fsonos_core::playback::PlayerPlayback;
    use fsonos_core::policy::Policy;
    use fsonos_core::store::{MemStore, Store};
    use fsonos_proto::net::Lan;
    use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
    use fsonos_types::PlayerId;
    use std::sync::mpsc::{self, Sender};

    struct Probe {
        active: PlayerId,
        seen: Sender<Option<u8>>,
    }

    impl DjEngine for Probe {
        fn act(
            &self,
            _: DjSpeakers<'_>,
            _: &mut dyn Store,
            _: DjAction,
            _: &dyn Clock,
        ) -> Result<OutcomeDto, Failure> {
            unreachable!("maintenance must not issue a DJ command")
        }

        fn steer(
            &self,
            _: DjSpeakers<'_>,
            _: &mut dyn Store,
            _: &DjSteer,
            _: &dyn Clock,
        ) -> Result<OutcomeDto, Failure> {
            unreachable!()
        }

        fn status(
            &self,
            _: DjSpeakers<'_>,
            _: &dyn Store,
            _: Option<u32>,
            _: &dyn Clock,
        ) -> Result<DjStatusDto, Failure> {
            Err(Failure::invalid("probe has no status"))
        }

        fn moods(
            &self,
            _: Option<DjSpeakers<'_>>,
            _: &dyn Store,
            _: &dyn Clock,
        ) -> Result<DjMoodsDto, Failure> {
            unreachable!()
        }

        fn feeds(&self, coordinator: &PlayerId) -> bool {
            *coordinator == self.active
        }

        fn on_playback(
            &self,
            at: DjSpeakers<'_>,
            _: &mut dyn Store,
            playback: &PlayerPlayback,
            _: &dyn Clock,
        ) {
            assert_eq!(*at.coordinator, self.active, "inactive feed was visited");
            let _ = self.seen.send(playback.volume);
        }
    }

    fn household() -> (SimHandle, Arc<Live>, PlayerId) {
        let sim = SimHousehold::builder()
            .s2([
                SimPlayerSpec::new("Den", SimModel::One),
                SimPlayerSpec::new("Office", SimModel::One),
            ])
            .spawn()
            .unwrap();
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let live = Arc::new(Live::start_with(
            Arc::new(sim.lan()),
            lan,
            LiveConfig::new(sim.players().iter().map(|p| p.ip).collect()),
        ));
        assert!(live.wait_ready(Duration::from_secs(10)));
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if sim.players().iter().all(|p| {
                live.player(&PlayerId(p.uuid.clone())).is_some_and(|p| {
                    p.transport.is_some() && p.volume.is_some() && p.group_volume.is_some()
                })
            }) {
                break;
            }
            assert!(Instant::now() < until, "initial GENA state did not arrive");
            std::thread::sleep(Duration::from_millis(10));
        }
        let id = PlayerId(sim.player("Den").unwrap().uuid.clone());
        (sim, live, id)
    }

    fn surface(sim: &SimHandle, live: &Arc<Live>, active: PlayerId) -> (Arc<Surface>, Receiver<Option<u8>>) {
        let (seen, received) = mpsc::channel();
        let surface = Surface::new(
            Box::new(sim.lan()),
            Box::new(|_| panic!("DJ maintenance must not survey the speakers")),
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_live(live)
        .with_dj(Box::new(Probe { active, seen }))
        .with_action_log(Box::new(MemStore::default()), "test");
        (Arc::new(surface), received)
    }

    #[test]
    fn follower_bootstraps_active_feeds_after_initial_events_already_arrived() {
        let (sim, live, active) = household();
        let expected = live.player(&active).unwrap().volume;
        let (surface, received) = surface(&sim, &live, active);
        // The real follower subscribes after Live already has every initial
        // state. No new playback change is needed to visit a restored feed.
        follow(&surface);
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            expected
        );
        drop(surface);
        drop(live);
        sim.shutdown();
    }

    #[test]
    fn idle_maintenance_uses_current_live_state_and_stops_with_the_surface() {
        let (sim, live, active) = household();
        let original = live.player(&active).unwrap().volume;
        let (surface, received) = surface(&sim, &live, active);
        let weak = Arc::downgrade(&surface);
        // Deliberately no events on this channel: only the timer can reach
        // the production handler, while Live receives real simulator GENA.
        let (_send, events) = mpsc::channel::<LiveEvent>();
        let worker = std::thread::spawn(move || {
            drive(&events, Duration::from_millis(10), |event| handle(&weak, event));
        });
        assert_eq!(
            received.recv_timeout(Duration::from_secs(2)).unwrap(),
            original
        );
        let changed = if original == Some(37) { 38 } else { 37 };
        fsonos_proto::control::set_volume(&sim.lan(), sim.player("Den").unwrap().ip, changed)
            .unwrap();
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            let remaining = until.saturating_duration_since(Instant::now());
            let seen = received.recv_timeout(remaining).unwrap();
            if seen == Some(changed) {
                break;
            }
        }
        drop(surface);
        worker.join().unwrap();
        drop(live);
        sim.shutdown();
    }
}
