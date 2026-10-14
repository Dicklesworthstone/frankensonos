use super::*;
use crate::events::{MAX_EVENT_READERS, RING, stream, try_stream};
use serde_json::json;
use std::sync::mpsc::{self, Sender};
use std::task::{Wake, Waker};
use std::time::Duration;

fn next(stream: &mut EventStream) -> Poll<Option<Vec<u8>>> {
    Pin::new(stream).poll_next(&mut Context::from_waker(Waker::noop()))
}

fn take(stream: &mut EventStream) -> String {
    match next(stream) {
        Poll::Ready(Some(bytes)) => String::from_utf8(bytes).unwrap(),
        other => panic!("expected an event, got {other:?}"),
    }
}

struct Notify(Sender<()>);
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        let _ = self.0.send(());
    }
}

#[test]
fn a_new_connection_replays_only_events_after_its_cursor_in_order() {
    let bus = Arc::new(EventBus::new());
    for n in 0..3 {
        bus.publish("zone.state", json!({ "n": n }));
    }
    let mut reader = stream(bus, 1);
    assert!(take(&mut reader).starts_with("id: 2\n"));
    assert!(take(&mut reader).starts_with("id: 3\n"));
    assert!(next(&mut reader).is_pending());
}

#[test]
fn a_publish_wakes_a_parked_reader_without_a_pump_thread() {
    let bus = Arc::new(EventBus::new());
    let mut reader = stream(Arc::clone(&bus), 0);
    let (sent, received) = mpsc::channel();
    let waker = Waker::from(Arc::new(Notify(sent)));
    assert!(
        Pin::new(&mut reader)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    assert_eq!(bus.wake.counts(), (1, 1, true));
    bus.publish("topology.changed", json!({}));
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(take(&mut reader).starts_with("id: 1\n"));
    assert_eq!(bus.wake.counts().1, 0);
}

#[test]
fn a_slow_reader_recovers_from_the_retained_journal_without_delaying_others() {
    let bus = Arc::new(EventBus::new());
    let mut slow = stream(Arc::clone(&bus), 0);
    let mut fast = stream(Arc::clone(&bus), 0);
    for n in 0..RING + 8 {
        let id = bus.publish("zone.state", json!({ "n": n }));
        assert!(take(&mut fast).starts_with(&format!("id: {id}\n")));
    }
    assert_eq!(bus.ring.lock().unwrap().events.len(), RING);
    assert_eq!(
        take(&mut slow),
        "event: events.reset\ndata: {\"oldest\":9}\n\n"
    );
    assert!(take(&mut slow).starts_with("id: 9\n"));
    assert!(next(&mut fast).is_pending());
}

#[test]
fn every_u64_resume_cursor_is_safe_even_before_the_first_event() {
    for after in [1, u64::MAX - 1, u64::MAX] {
        let bus = Arc::new(EventBus::new());
        assert_eq!(bus.after(after), Err(1));
        let mut reader = stream(Arc::clone(&bus), after);
        assert!(take(&mut reader).contains("events.reset"));
        assert!(next(&mut reader).is_pending());
        bus.publish("zone.state", json!({}));
        assert!(take(&mut reader).starts_with("id: 1\n"));
    }
}

#[test]
fn idle_readers_emit_heartbeats_without_advancing_the_event_cursor() {
    let bus = Arc::new(EventBus::new());
    let mut reader = stream(bus, 0);
    reader.subscription.as_mut().unwrap().heartbeat_at = Instant::now();
    assert_eq!(take(&mut reader), ": heartbeat\n\n");
    assert_eq!(reader.subscription.as_ref().unwrap().after, 0);
    assert!(next(&mut reader).is_pending());
}

#[test]
fn a_ready_event_takes_precedence_over_a_due_heartbeat() {
    let bus = Arc::new(EventBus::new());
    let mut reader = stream(Arc::clone(&bus), 0);
    reader.subscription.as_mut().unwrap().heartbeat_at = Instant::now();
    bus.publish("zone.state", json!({}));
    assert!(take(&mut reader).starts_with("id: 1\n"));
    assert!(next(&mut reader).is_pending());
}

#[test]
fn dropping_the_body_immediately_releases_its_lease_and_waker() {
    let bus = Arc::new(EventBus::new());
    let mut reader = stream(Arc::clone(&bus), 0);
    assert!(next(&mut reader).is_pending());
    assert_eq!(bus.wake.counts().0, 1);
    drop(reader);
    assert_eq!(bus.wake.counts().0, 0);
    assert_eq!(bus.wake.counts().1, 0);
    let unpolled = stream(Arc::clone(&bus), 0);
    drop(unpolled);
    assert_eq!(bus.wake.counts().0, 0);
}

#[test]
fn admission_is_bounded_and_a_released_slot_can_be_reused() {
    let bus = Arc::new(EventBus::new());
    let mut readers: Vec<_> = (0..MAX_EVENT_READERS)
        .map(|_| try_stream(Arc::clone(&bus), 0).ok().unwrap())
        .collect();
    let failure = try_stream(Arc::clone(&bus), 0).err().unwrap();
    assert_eq!(failure.status(), 503);
    drop(readers.pop());
    assert!(try_stream(Arc::clone(&bus), 0).is_ok());
}

#[test]
fn the_infallible_api_reports_admission_failure_instead_of_silent_eof() {
    let bus = Arc::new(EventBus::new());
    let _readers: Vec<_> = (0..MAX_EVENT_READERS)
        .map(|_| stream(Arc::clone(&bus), 0))
        .collect();
    let mut refused = stream(bus, 0);
    assert!(take(&mut refused).contains("event: events.unavailable\n"));
    assert_eq!(next(&mut refused), Poll::Ready(None));
}
