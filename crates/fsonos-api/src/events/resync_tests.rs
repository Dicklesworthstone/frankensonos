//! Exercise upstream discontinuity through the actual journal and consumers.

use super::*;
use asupersync::stream::Stream;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

#[derive(Default)]
struct CountWake(AtomicUsize);

impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.fetch_add(1, Ordering::SeqCst);
    }
}

#[test]
fn upstream_loss_resets_even_a_reader_at_the_previously_newest_event() {
    let bus = EventBus::new();
    let last = bus.publish("zone.state", json!({ "volume": 12 }));
    bus.publish_live(&LiveEvent::Resync, &[]);
    assert_eq!(bus.after(last), Err(last + 2));
    let answer = poll(&bus, last, Duration::ZERO);
    assert_eq!(answer.reset, Some(last + 2));
    assert_eq!(answer.last_id, last + 1);
    assert!(answer.events.is_empty());
    // Acknowledging that gap must not repeatedly reset an idle reader.
    let next = poll(&bus, answer.last_id, Duration::ZERO);
    assert_eq!(next.reset, None);
    assert_eq!(next.last_id, answer.last_id);
    assert!(next.events.is_empty());
}

#[test]
fn a_gap_before_the_first_publication_is_not_a_quiet_empty_house() {
    let bus = EventBus::new();
    bus.publish_live(&LiveEvent::Resync, &[]);
    let answer = poll(&bus, 0, Duration::ZERO);
    assert_eq!(answer.reset, Some(2));
    assert_eq!(answer.last_id, 1);
    assert_eq!(bus.last_id(), 1);
}

#[test]
fn late_reconnects_still_see_loss_and_never_replay_pre_gap_deltas() {
    let bus = EventBus::new();
    bus.publish("action.logged", json!({ "result": "earlier action" }));
    let last = bus.publish("zone.state", json!({ "volume": 12 }));
    bus.publish_live(&LiveEvent::Resync, &[]);
    let current = bus.publish("zone.state", json!({ "volume": 37 }));
    assert_eq!(current, last + 2);
    for old in 0..=last {
        let answer = poll(&bus, old, Duration::ZERO);
        assert_eq!(answer.reset, Some(current));
        assert_eq!(answer.last_id, current);
        assert_eq!(answer.events.len(), 1);
        assert_eq!(answer.events[0].data["volume"], 37);
    }
    assert_eq!(poll(&bus, current, Duration::ZERO).reset, None);
}

#[test]
fn repeated_upstream_gaps_remain_visible_after_each_acknowledgement() {
    let bus = EventBus::new();
    let mut cursor = 0;
    for _ in 0..10 {
        bus.publish_live(&LiveEvent::Resync, &[]);
        let answer = poll(&bus, cursor, Duration::ZERO);
        assert!(answer.reset.is_some());
        assert!(answer.last_id > cursor);
        cursor = answer.last_id;
        assert_eq!(poll(&bus, cursor, Duration::ZERO).reset, None);
    }
}

#[test]
fn a_parked_sse_reader_wakes_for_a_gap_without_a_later_event() {
    let bus = Arc::new(EventBus::new());
    let mut stream = try_stream(Arc::clone(&bus), 0).unwrap();
    let count = Arc::new(CountWake::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    bus.publish_live(&LiveEvent::Resync, &[]);
    assert!(count.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Some(frame)) = Pin::new(&mut stream).poll_next(&mut cx) else {
        panic!("the missing publication must reset a parked stream");
    };
    assert_eq!(frame, reset(2));
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    let id = bus.publish("zone.state", json!({ "volume": 37 }));
    let Poll::Ready(Some(frame)) = Pin::new(&mut stream).poll_next(&mut cx) else {
        panic!("the next delta must follow the acknowledged reset");
    };
    assert_eq!(frame, bus.after(id - 1).unwrap()[0].frame());
}

#[test]
fn a_waiting_http_poll_wakes_for_loss_with_its_coherent_reset_cursor() {
    let bus = Arc::new(EventBus::new());
    let mut stream = polling::PollStream::open(Arc::clone(&bus), 0, MAX_WAIT).unwrap();
    let count = Arc::new(CountWake::default());
    let waker = Waker::from(Arc::clone(&count));
    let mut cx = Context::from_waker(&waker);
    assert!(Pin::new(&mut stream).poll_next(&mut cx).is_pending());
    bus.publish_live(&LiveEvent::Resync, &[]);
    assert!(count.0.load(Ordering::SeqCst) > 0);
    let Poll::Ready(Some(bytes)) = Pin::new(&mut stream).poll_next(&mut cx) else {
        panic!("upstream loss must complete the HTTP long poll");
    };
    let answer: EventsDto = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(answer.reset, Some(2));
    assert_eq!(answer.last_id, 1);
    assert!(answer.events.is_empty());
    assert!(matches!(
        Pin::new(&mut stream).poll_next(&mut cx),
        Poll::Ready(None)
    ));
}
