use super::*;
use crate::events::{EventsDto, MAX_EVENT_READERS, RING, poll, stream};
use serde_json::json;
use std::sync::mpsc::{self, Sender};
use std::task::{Wake, Waker};

fn next(stream: &mut PollStream) -> Poll<Option<Vec<u8>>> {
    Pin::new(stream).poll_next(&mut Context::from_waker(Waker::noop()))
}

fn take(stream: &mut PollStream) -> EventsDto {
    match next(stream) {
        Poll::Ready(Some(bytes)) => serde_json::from_slice(&bytes).unwrap(),
        other => panic!("expected a JSON snapshot, got {other:?}"),
    }
}

struct Notify(Sender<()>);
impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        let _ = self.0.send(());
    }
}

#[test]
fn a_poll_yields_one_json_body_and_releases_capacity_before_eof() {
    let bus = Arc::new(EventBus::new());
    bus.publish("zone.state", json!({ "volume": 12 }));
    bus.publish("zone.state", json!({ "volume": 15 }));
    let mut reader = PollStream::open(Arc::clone(&bus), 1, MAX_WAIT)
        .ok()
        .unwrap();
    let snapshot = take(&mut reader);
    assert_eq!(snapshot.last_id, 2);
    assert_eq!(snapshot.events.len(), 1);
    assert_eq!(snapshot.events[0].data["volume"], 15);
    assert_eq!(bus.wake.counts().0, 0);
    assert_eq!(next(&mut reader), Poll::Ready(None));
}

#[test]
fn a_quiet_poll_wakes_on_publication_and_returns_the_same_wire_dto() {
    let bus = Arc::new(EventBus::new());
    let mut reader = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    let (sent, received) = mpsc::channel();
    let waker = Waker::from(Arc::new(Notify(sent)));
    assert!(
        Pin::new(&mut reader)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    bus.publish("action.logged", json!({ "result": "paused" }));
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(take(&mut reader), poll(&bus, 0, Duration::ZERO));
}

#[test]
fn a_quiet_poll_times_out_without_an_event_or_a_dedicated_thread() {
    let bus = Arc::new(EventBus::new());
    let mut reader = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    reader.deadline = Instant::now() + Duration::from_millis(20);
    let (sent, received) = mpsc::channel();
    let waker = Waker::from(Arc::new(Notify(sent)));
    assert!(
        Pin::new(&mut reader)
            .poll_next(&mut Context::from_waker(&waker))
            .is_pending()
    );
    received.recv_timeout(Duration::from_secs(2)).unwrap();
    let answer = take(&mut reader);
    assert!(answer.events.is_empty());
    assert_eq!(answer.last_id, 0);
    assert_eq!(answer.reset, None);
    assert_eq!(bus.wake.counts().0, 0);
}

#[test]
fn dropping_a_waiting_poll_releases_the_reader_and_its_waker() {
    let bus = Arc::new(EventBus::new());
    let mut reader = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    assert!(next(&mut reader).is_pending());
    assert_eq!(bus.wake.counts().1, 1);
    drop(reader);
    assert_eq!(bus.wake.counts().0, 0);
    assert_eq!(bus.wake.counts().1, 0);
}

#[test]
fn waiting_polls_and_sse_connections_share_the_same_admission_budget() {
    let bus = Arc::new(EventBus::new());
    let mut readers: Vec<_> = (0..MAX_EVENT_READERS)
        .map(|_| stream(Arc::clone(&bus), 0))
        .collect();
    assert!(PollStream::open(Arc::clone(&bus), 0, MAX_WAIT).is_err());
    drop(readers.pop());
    let poll = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    assert_eq!(bus.wake.counts().0, MAX_EVENT_READERS);
    drop(poll);
    assert!(crate::events::try_stream(Arc::clone(&bus), 0).is_ok());
}

#[test]
fn reset_events_and_last_id_come_from_the_same_retained_snapshot() {
    let bus = Arc::new(EventBus::new());
    for n in 0..RING + 12 {
        bus.publish("zone.state", json!({ "n": n }));
    }
    let mut reader = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    let answer = take(&mut reader);
    assert_eq!(answer.reset, Some(13));
    assert_eq!(answer.events[0].id, 13);
    assert_eq!(answer.events.len(), RING);
    assert_eq!(answer.events.last().unwrap().id, answer.last_id);
    assert_eq!(answer, poll(&bus, 0, Duration::ZERO));
}

#[test]
fn future_cursors_reset_even_when_no_events_have_arrived() {
    let bus = Arc::new(EventBus::new());
    let mut reader = PollStream::open(Arc::clone(&bus), u64::MAX, MAX_WAIT)
        .ok()
        .unwrap();
    let answer = take(&mut reader);
    assert_eq!(answer.reset, Some(1));
    assert_eq!(answer.last_id, 0);
    assert!(answer.events.is_empty());
    assert_eq!(answer, poll(&bus, u64::MAX, Duration::ZERO));
}

#[test]
fn long_waits_are_clamped_and_zero_waits_do_not_park() {
    let bus = Arc::new(EventBus::new());
    let before = Instant::now();
    let reader = PollStream::open(Arc::clone(&bus), 0, Duration::MAX)
        .ok()
        .unwrap();
    assert!(reader.deadline >= before + MAX_WAIT);
    assert!(reader.deadline <= Instant::now() + MAX_WAIT);
    drop(reader);
    let mut immediate = PollStream::open(Arc::clone(&bus), 0, Duration::ZERO)
        .ok()
        .unwrap();
    assert!(take(&mut immediate).events.is_empty());
    assert_eq!(bus.wake.counts().1, 0);
}

#[test]
fn events_arriving_after_a_completed_poll_are_left_for_the_next_cursor() {
    let bus = Arc::new(EventBus::new());
    bus.publish("zone.state", json!({}));
    let mut first = PollStream::open(Arc::clone(&bus), 0, MAX_WAIT)
        .ok()
        .unwrap();
    let cursor = take(&mut first).last_id;
    bus.publish("zone.state", json!({}));
    assert_eq!(next(&mut first), Poll::Ready(None));
    let mut second = PollStream::open(bus, cursor, MAX_WAIT).ok().unwrap();
    assert_eq!(take(&mut second).events[0].id, 2);
}

#[test]
fn snapshots_remain_coherent_while_a_publisher_evicts_old_events() {
    let bus = Arc::new(EventBus::new());
    let barrier = Arc::new(std::sync::Barrier::new(2));
    let publishing = Arc::clone(&bus);
    let go = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        go.wait();
        for n in 0..RING * 4 {
            publishing.publish("zone.state", json!({ "n": n }));
        }
    });
    barrier.wait();
    for _ in 0..128 {
        let answer = poll(&bus, 0, Duration::ZERO);
        assert!(answer.events.len() <= RING);
        if let Some(oldest) = answer.reset {
            assert_eq!(answer.events.first().unwrap().id, oldest);
        }
        if let Some(last) = answer.events.last() {
            assert_eq!(answer.last_id, last.id);
        }
        for adjacent in answer.events.windows(2) {
            assert_eq!(adjacent[1].id, adjacent[0].id + 1);
        }
    }
    worker.join().unwrap();
}
