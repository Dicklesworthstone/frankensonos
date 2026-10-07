//! The daemon's event stream: what changed in the house, as numbered events
//! a client can resume from (`GET /events`, server-sent events).
//!
//! One [`EventBus`] per daemon. The live model's changes are published to it
//! ([`EventBus::publish_live`]), and so is every logged action. The bus keeps
//! the last [`RING`] events, so a client that reconnects with
//! `Last-Event-ID` (or `?since=<id>`) gets what it missed. One that has
//! fallen further behind gets an `events.reset` event first and should read
//! the state afresh (`GET /zones`).
//!
//! | `event:` | `data:` |
//! |---|---|
//! | `zone.state` | `{player, room, transport?, track?, volume?, mute?, group_volume?}`: only what changed |
//! | `topology.changed` | `{households, rooms}` (read `GET /zones` for the new shape) |
//! | `player.health` | `{player, room, health}`: `healthy`, `degraded` or `offline` |
//! | `action.logged` | the action as `GET /actions` shows it |
//! | `events.reset` | `{oldest}`: events before it are gone |
//!
//! While nothing happens the stream sends a `: heartbeat` comment every
//! [`HEARTBEAT`], so proxies and clients can tell a quiet house from a dead
//! connection.
//!
//! The frames are written here: `fastapi_core::sse` exists in fastapi_rust's
//! source at the pinned revision but is not compiled into the crate (no
//! `mod sse`), so the stream goes out through the public streaming body
//! ([`fastapi::ResponseBody::stream`]) instead.

use asupersync::stream::Stream;
use fastapi::{Response, ResponseBody};
use fsonos_core::HouseholdState;
use fsonos_core::live::{Live, LiveEvent};
use fsonos_core::reconcile::Health;
use fsonos_types::PlayerId;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use crate::zones::transport_state_name;

/// How many events the bus keeps for clients that reconnect.
pub const RING: usize = 1024;

/// How often a quiet stream sends a heartbeat comment.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// One event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// Increasing from 1; the SSE `id:`.
    pub id: u64,
    /// The SSE `event:` (see the module docs).
    pub kind: &'static str,
    pub data: Value,
}

impl Event {
    /// The SSE frame: `id:`, `event:` and one `data:` line per line.
    #[must_use]
    pub fn frame(&self) -> Vec<u8> {
        frame(Some(self.id), self.kind, &self.data.to_string())
    }
}

fn frame(id: Option<u64>, kind: &str, data: &str) -> Vec<u8> {
    let mut out = String::new();
    if let Some(id) = id {
        out.push_str("id: ");
        out.push_str(&id.to_string());
        out.push('\n');
    }
    out.push_str("event: ");
    out.push_str(kind);
    out.push('\n');
    for line in data.split('\n') {
        out.push_str("data: ");
        out.push_str(line);
        out.push('\n');
    }
    out.push('\n');
    out.into_bytes()
}

/// `events.reset`: events before `oldest` are gone; read the state afresh.
fn reset(oldest: u64) -> Vec<u8> {
    frame(
        None,
        "events.reset",
        &json!({ "oldest": oldest }).to_string(),
    )
}

const HEARTBEAT_FRAME: &[u8] = b": heartbeat\n\n";

/// The daemon's event bus; see the module docs.
#[derive(Debug, Default)]
pub struct EventBus {
    ring: Mutex<Ring>,
    arrived: Condvar,
}

#[derive(Debug, Default)]
struct Ring {
    /// The id the next event gets, less one.
    last: u64,
    events: VecDeque<Event>,
}

impl Ring {
    /// The retained events after `after`, or the oldest retained id when
    /// some event after `after` has already been dropped.
    fn after(&self, after: u64) -> Result<Vec<Event>, u64> {
        let oldest = self.events.front().map_or(self.last + 1, |e| e.id);
        if after + 1 < oldest {
            return Err(oldest);
        }
        Ok(self
            .events
            .iter()
            .filter(|e| e.id > after)
            .cloned()
            .collect())
    }
}

impl EventBus {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Publish an event; returns its id.
    pub fn publish(&self, kind: &'static str, data: Value) -> u64 {
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        ring.last += 1;
        let id = ring.last;
        ring.events.push_back(Event { id, kind, data });
        while ring.events.len() > RING {
            ring.events.pop_front();
        }
        drop(ring);
        self.arrived.notify_all();
        id
    }

    /// The id of the newest event (0 before any).
    #[must_use]
    pub fn last_id(&self) -> u64 {
        self.ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last
    }

    /// The retained events after `after`; `Err(oldest)` when some of them
    /// have already left the ring.
    pub fn after(&self, after: u64) -> Result<Vec<Event>, u64> {
        self.ring
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .after(after)
    }

    /// [`Self::after`], waiting up to `timeout` for there to be any.
    pub fn wait_after(&self, after: u64, timeout: Duration) -> Result<Vec<Event>, u64> {
        let deadline = Instant::now() + timeout;
        let mut ring = self.ring.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            let found = ring.after(after);
            let left = deadline.saturating_duration_since(Instant::now());
            if !matches!(&found, Ok(events) if events.is_empty()) || left.is_zero() {
                return found;
            }
            ring = self
                .arrived
                .wait_timeout(ring, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// Publish what the live model saw, naming rooms from `households`.
    pub fn publish_live(&self, event: &LiveEvent, households: &[HouseholdState]) {
        match event {
            LiveEvent::Playback { player, changes } => {
                let mut data = player_fields(player, households);
                if let Some((_, now)) = changes.transport {
                    data.insert("transport".into(), json!(transport_state_name(now)));
                }
                if let Some((_, track)) = &changes.track {
                    data.insert("track".into(), json!(track));
                }
                if let Some(volume) = changes.volume {
                    data.insert("volume".into(), json!(volume));
                }
                if let Some(mute) = changes.mute {
                    data.insert("mute".into(), json!(mute));
                }
                if let Some(volume) = changes.group_volume {
                    data.insert("group_volume".into(), json!(volume));
                }
                self.publish("zone.state", Value::Object(data));
            }
            LiveEvent::Topology => {
                let rooms: usize = households.iter().map(|h| h.rooms.len()).sum();
                self.publish(
                    "topology.changed",
                    json!({ "households": households.len(), "rooms": rooms }),
                );
            }
            LiveEvent::Health { player, health } => {
                let mut data = player_fields(player, households);
                let health = match health {
                    Health::Healthy => "healthy",
                    Health::Degraded => "degraded",
                    Health::Offline => "offline",
                };
                data.insert("health".into(), json!(health));
                self.publish("player.health", Value::Object(data));
            }
        }
    }
}

/// Publish every change `live` sees to `bus`, on a thread that ends with
/// the model.
pub fn feed(bus: &Arc<EventBus>, live: &Arc<Live>) {
    let changes = live.subscribe();
    let model: Weak<Live> = Arc::downgrade(live);
    let bus = Arc::clone(bus);
    let _ = std::thread::Builder::new()
        .name("fsonos-event-bus".into())
        .spawn(move || {
            while let Ok(change) = changes.recv() {
                let households = model.upgrade().map(|l| l.households()).unwrap_or_default();
                bus.publish_live(&change, &households);
            }
        });
}

/// `{player, room}` for `player` (`room` when the households know it).
fn player_fields(player: &PlayerId, households: &[HouseholdState]) -> Map<String, Value> {
    let mut data = Map::new();
    data.insert("player".into(), json!(player.0));
    let room = households
        .iter()
        .flat_map(|h| &h.rooms)
        .find(|r| r.players.contains(player) || r.missing.contains(player));
    if let Some(room) = room {
        data.insert("room".into(), json!(room.name));
    }
    data
}

/// `GET /events`: the server-sent events after `after`, as they happen.
#[must_use]
pub fn response(bus: Arc<EventBus>, after: u64) -> Response {
    Response::ok()
        .header("content-type", b"text/event-stream".to_vec())
        .header("cache-control", b"no-cache".to_vec())
        .header("x-accel-buffering", b"no".to_vec())
        .body(ResponseBody::stream(stream(bus, after)))
}

/// The SSE frames for a client that has seen everything up to `after`, as
/// they happen. Each client gets a thread that waits on the bus; it ends
/// within a [`HEARTBEAT`] of the client going away.
#[must_use]
pub fn stream(bus: Arc<EventBus>, after: u64) -> EventStream {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let feed = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("fsonos-events".into())
        .spawn(move || pump(&bus, after, &feed));
    if spawned.is_err() {
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finished = true;
    }
    EventStream { shared }
}

/// Feed `shared` from `bus` until the stream is dropped.
fn pump(bus: &EventBus, mut after: u64, shared: &Mutex<Shared>) {
    loop {
        let next = bus.wait_after(after, HEARTBEAT);
        let mut s = shared.lock().unwrap_or_else(PoisonError::into_inner);
        if s.closed {
            return;
        }
        match next {
            Ok(events) if events.is_empty() => s.queue.push_back(HEARTBEAT_FRAME.to_vec()),
            Ok(events) => {
                for event in events {
                    after = event.id;
                    s.queue.push_back(event.frame());
                }
            }
            Err(oldest) => {
                s.queue.push_back(reset(oldest));
                after = oldest - 1;
            }
        }
        // A client that stopped reading: start it over rather than buffer.
        if s.queue.len() > RING {
            s.queue.clear();
            s.queue.push_back(reset(after + 1));
        }
        if let Some(waker) = s.waker.take() {
            waker.wake();
        }
    }
}

#[derive(Default)]
struct Shared {
    queue: VecDeque<Vec<u8>>,
    waker: Option<Waker>,
    /// The client went away.
    closed: bool,
    /// No pump is feeding the queue.
    finished: bool,
}

/// The body of `GET /events`; see [`stream`].
pub struct EventStream {
    shared: Arc<Mutex<Shared>>,
}

impl Stream for EventStream {
    type Item = Vec<u8>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Vec<u8>>> {
        let mut s = self.shared.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(event) = s.queue.pop_front() {
            return Poll::Ready(Some(event));
        }
        if s.finished {
            return Poll::Ready(None);
        }
        s.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for EventStream {
    fn drop(&mut self) {
        self.shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .closed = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn publish(bus: &EventBus, n: usize) {
        for i in 0..n {
            bus.publish("zone.state", json!({ "n": i }));
        }
    }

    #[test]
    fn ids_increase_and_resume_returns_what_was_missed() {
        let bus = EventBus::new();
        assert_eq!(bus.last_id(), 0);
        assert_eq!(bus.after(0), Ok(Vec::new()));
        publish(&bus, 3);
        assert_eq!(bus.last_id(), 3);
        let missed = bus.after(1).unwrap();
        assert_eq!(missed.iter().map(|e| e.id).collect::<Vec<_>>(), [2, 3]);
        assert_eq!(bus.after(3), Ok(Vec::new()));
    }

    #[test]
    fn the_ring_keeps_the_last_thousand_and_reports_a_gap() {
        let bus = EventBus::new();
        publish(&bus, RING + 10);
        // Events 1..=10 are gone.
        assert_eq!(bus.after(0), Err(11));
        assert_eq!(bus.after(9), Err(11));
        // Resuming from just before the oldest kept one loses nothing.
        let all = bus.after(10).unwrap();
        assert_eq!(all.len(), RING);
        assert_eq!(all[0].id, 11);
    }

    #[test]
    fn waiting_returns_as_soon_as_an_event_arrives() {
        let bus = Arc::new(EventBus::new());
        let publisher = Arc::clone(&bus);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            publisher.publish("topology.changed", json!({}));
        });
        let started = Instant::now();
        let got = bus.wait_after(0, Duration::from_secs(5)).unwrap();
        assert_eq!(got.len(), 1);
        assert!(started.elapsed() < Duration::from_secs(2));
        t.join().unwrap();
        // Nothing new: an empty answer once the timeout passes.
        assert_eq!(bus.wait_after(1, Duration::from_millis(20)), Ok(Vec::new()));
    }

    #[test]
    fn frames_follow_the_sse_wire_format() {
        let event = Event {
            id: 7,
            kind: "zone.state",
            data: json!({ "volume": 30 }),
        };
        assert_eq!(
            String::from_utf8(event.frame()).unwrap(),
            "id: 7\nevent: zone.state\ndata: {\"volume\":30}\n\n"
        );
        assert_eq!(
            String::from_utf8(frame(None, "x", "a\nb")).unwrap(),
            "event: x\ndata: a\ndata: b\n\n"
        );
    }

    #[test]
    fn live_changes_name_the_room_and_only_what_changed() {
        let bus = EventBus::new();
        let households = crate::zones::fixtures::households();
        let player = crate::zones::fixtures::id("RINCON_DEN");
        let changes = fsonos_core::playback::Changes {
            volume: Some(30),
            ..Default::default()
        };
        bus.publish_live(&LiveEvent::Playback { player, changes }, &households);
        let event = &bus.after(0).unwrap()[0];
        assert_eq!(event.kind, "zone.state");
        assert_eq!(event.data["volume"], 30);
        assert!(event.data["room"].is_string(), "{}", event.data);
        assert!(event.data.get("transport").is_none());
    }
}
