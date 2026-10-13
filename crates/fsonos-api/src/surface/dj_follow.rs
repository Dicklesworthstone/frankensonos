//! Drive the DJ from live events, with bounded maintenance for quiet queues.

use super::Surface;
use fsonos_core::live::LiveEvent;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
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
        .spawn(move || {
            drive(&changes, MAINTENANCE, |event| {
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
                        // A restored feed may have missed the initial events
                        // before follow started. Always use the latest model,
                        // never a saved event or a direct household survey.
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
            });
        })
    {
        tracing::error!(%error, "could not start the DJ queue follower");
    }
}

/// `None` asks for maintenance; `Some` delivers an event. Returning false
/// stops the worker. Run once immediately, then at most once per interval,
/// including while a busy event stream never lets recv_timeout time out.
fn drive<T>(
    events: &Receiver<T>,
    interval: Duration,
    mut handle: impl FnMut(Option<T>) -> bool,
) {
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
            assert_eq!(receive.recv_timeout(Duration::from_secs(2)).unwrap(), expected);
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
