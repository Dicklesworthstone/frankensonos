use super::*;
use std::sync::mpsc::{self, Receiver, Sender};
use std::task::Wake;
use std::time::Duration;

struct Notify(Sender<()>);

impl Wake for Notify {
    fn wake(self: Arc<Self>) {
        let _ = self.0.send(());
    }
}

fn notifier() -> (Waker, Receiver<()>) {
    let (tx, rx) = mpsc::channel();
    (Waker::from(Arc::new(Notify(tx))), rx)
}

fn later() -> Instant {
    Instant::now() + Duration::from_secs(60)
}

fn stopped(hub: &WakeHub) {
    let state = hub.state.lock().unwrap();
    let (state, _) = hub
        .changed
        .wait_timeout_while(state, Duration::from_secs(2), |state| state.running)
        .unwrap();
    assert!(!state.running, "deadline worker did not retire");
}

#[test]
fn readers_share_one_worker_and_release_capacity_on_drop() {
    let hub = Arc::new(WakeHub::default());
    let mut readers = (0..MAX_READERS)
        .map(|_| hub.reader().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(hub.counts(), (MAX_READERS, 0, true));
    assert!(hub.reader().is_err());
    drop(readers.pop());
    let replacement = hub.reader().unwrap();
    assert_eq!(hub.counts().0, MAX_READERS);
    drop(replacement);
    drop(readers);
    stopped(&hub);
    assert_eq!(hub.counts(), (0, 0, false));
}

#[test]
fn publication_wakes_each_parked_reader_once() {
    let hub = Arc::new(WakeHub::default());
    let a = hub.reader().unwrap();
    let b = hub.reader().unwrap();
    let (wa, ra) = notifier();
    let (wb, rb) = notifier();
    let _ = a.park(wa, later());
    let _ = b.park(wb, later());
    hub.notify();
    ra.recv_timeout(Duration::from_secs(2)).unwrap();
    rb.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(hub.counts().1, 0);
    hub.notify();
    assert!(ra.try_recv().is_err());
    assert!(rb.try_recv().is_err());
}

#[test]
fn a_repoll_replaces_the_old_waker_without_growing_the_registry() {
    let hub = Arc::new(WakeHub::default());
    let reader = hub.reader().unwrap();
    let (old, old_rx) = notifier();
    let (new, new_rx) = notifier();
    let _ = reader.park(old, later());
    let _ = reader.park(new, later());
    assert_eq!(hub.counts().1, 1);
    hub.notify();
    new_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(old_rx.try_recv().is_err());
}

#[test]
fn deadlines_wake_without_events_and_do_not_spin() {
    let hub = Arc::new(WakeHub::default());
    let reader = hub.reader().unwrap();
    let (waker, rx) = notifier();
    let _ = reader.park(waker, Instant::now() + Duration::from_millis(20));
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(hub.counts().1, 0);
    assert!(rx.try_recv().is_err());
}

#[test]
fn an_earlier_deadline_interrupts_the_workers_existing_wait() {
    let hub = Arc::new(WakeHub::default());
    let late = hub.reader().unwrap();
    let early = hub.reader().unwrap();
    let (late_waker, late_rx) = notifier();
    let _ = late.park(late_waker, later());
    let (early_waker, early_rx) = notifier();
    let _ = early.park(early_waker, Instant::now());
    early_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert!(late_rx.try_recv().is_err());
}

#[test]
fn dropping_a_parked_reader_retires_the_worker_without_its_deadline() {
    let hub = Arc::new(WakeHub::default());
    let reader = hub.reader().unwrap();
    let (waker, rx) = notifier();
    let _ = reader.park(waker, later());
    drop(reader);
    stopped(&hub);
    assert_eq!(hub.counts(), (0, 0, false));
    assert!(rx.try_recv().is_err());
    // A subsequent connection starts delivery again, rather than inheriting
    // a stale running flag from the previous worker.
    let again = hub.reader().unwrap();
    let (waker, rx) = notifier();
    let _ = again.park(waker, later());
    hub.notify();
    rx.recv_timeout(Duration::from_secs(2)).unwrap();
}

#[test]
fn ready_readers_discard_old_deadlines() {
    let hub = Arc::new(WakeHub::default());
    let reader = hub.reader().unwrap();
    let (waker, rx) = notifier();
    let _ = reader.park(waker, later());
    reader.clear();
    assert_eq!(hub.counts().1, 0);
    hub.notify();
    assert!(rx.try_recv().is_err());
}

struct Reentrant {
    hub: Arc<WakeHub>,
    sent: Sender<usize>,
}

impl Wake for Reentrant {
    fn wake(self: Arc<Self>) {
        let _ = self.sent.send(self.hub.counts().0);
    }
}

#[test]
fn callbacks_run_outside_the_registry_lock() {
    let hub = Arc::new(WakeHub::default());
    let reader = hub.reader().unwrap();
    let (sent, received) = mpsc::channel();
    let waker = Waker::from(Arc::new(Reentrant {
        hub: Arc::clone(&hub),
        sent,
    }));
    let _ = reader.park(waker, later());
    let publisher = Arc::clone(&hub);
    let join = std::thread::spawn(move || publisher.notify());
    assert_eq!(received.recv_timeout(Duration::from_secs(2)).unwrap(), 1);
    join.join().unwrap();
}
