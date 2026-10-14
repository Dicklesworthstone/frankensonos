//! The daemon's event stream: what changed in the house, as numbered events
//! a client can resume from (`GET /events`, server-sent events).
//!
//! One [`EventBus`] per daemon. The live model's changes are published to it
//! ([`EventBus::publish_live`]), and so is every logged action and every
//! change in what the DJ plays or picks. The bus keeps the last [`RING`]
//! events, so a client that reconnects with
//! `Last-Event-ID` (or `?since=<id>`) gets what it missed. One that has
//! fallen further behind gets an `events.reset` event first and should read
//! the state afresh (`GET /zones`).
//!
//! | `event:` | `data:` |
//! |---|---|
//! | `zone.state` | `{player, room, transport?, track?, volume?, mute?, group_volume?, queue_length?, shuffle?, repeat?, play_mode?}`: only what changed (`repeat`: `off`, `all` or `one`; `play_mode` as `set_play_mode` takes it) |
//! | `topology.changed` | `{households, rooms}` (read `GET /zones` for the new shape) |
//! | `player.health` | `{player, room, health}`: `healthy`, `degraded` or `offline` |
//! | `action.logged` | the action as `GET /actions` shows it |
//! | `dj.status` | `{player, zone, running, now?, next, steering, problem?}` as `GET /zones/{room}/dj` shows it: what the DJ plays, the works it picked next and why, its steering, and what it last ran into feeding the queue (`problem`), sent when that changes (a pick, a new work, a start, skip, stop or steer, a top-up that fails) |
//! | `events.reset` | `{oldest}`: events before it are gone |
//!
//! While nothing happens the stream sends a `: heartbeat` comment every
//! [`HEARTBEAT`], so proxies and clients can tell a quiet house from a dead
//! connection.
//!
//! A client that can't read a stream (an agent's tool call, say) asks
//! `GET /events?wait=<secs>` instead, or calls the `events` tool
//! ([`Surface::events_after`]): one JSON answer with the events after `since`,
//! waiting up to `wait` seconds (at most [`MAX_WAIT`]) for one.
//!
//! The frames are written here: `fastapi_core::sse` exists in fastapi_rust's
//! source at the pinned revision but is not compiled into the crate (no
//! `mod sse`), so the stream goes out through the public streaming body
//! ([`fastapi::ResponseBody::stream`]) instead.

use asupersync::stream::Stream;
use fastapi::{JsonSchema, Response, ResponseBody, fastapi_openapi};
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

use crate::failure::Failure;
use crate::surface::Surface;
use crate::zones::transport_state_name;
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};

/// How many events the bus keeps for clients that reconnect.
pub const RING: usize = 1024;

/// How often a quiet stream sends a heartbeat comment.
pub const HEARTBEAT: Duration = Duration::from_secs(15);

/// The longest a long poll (`GET /events?wait=`, the `events` tool) waits.
pub const MAX_WAIT: Duration = Duration::from_mins(1);

/// One event, as a long poll answers it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventDto {
    /// Increasing from 1: pass the newest you have as `since`.
    pub id: u64,
    /// What happened: `zone.state`, `topology.changed`, `player.health`,
    /// `action.logged` or `dj.status` (see the event stream's table).
    pub event: String,
    /// The details: for `zone.state`, only what changed.
    pub data: Value,
}

/// The house's changes after `since`, in one answer (`GET /events?wait=`,
/// the `events` tool).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct EventsDto {
    /// Oldest first.
    pub events: Vec<EventDto>,
    /// The newest event id: pass it as `since` next time.
    pub last_id: u64,
    /// Set when events after `since` are gone (too many happened): the
    /// oldest one kept. Read the state afresh (`list_zones`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<u64>,
}

impl Surface {
    /// The house's changes after `since` (`events`, read-only; from now when
    /// `None`), waiting up to `wait` (at most [`MAX_WAIT`]) for one.
    ///
    /// # Errors
    /// `POLICY_DENIED`, or `NOT_IMPLEMENTED` off the daemon's live model.
    /// Blocks the calling thread for the wait: a server answers with
    /// [`poll_response`] instead.
    pub fn events_after(
        &self,
        client: &Client,
        since: Option<u64>,
        wait: Duration,
    ) -> Result<EventsDto, Failure> {
        let bus = self.events(client)?;
        let since = since.unwrap_or_else(|| bus.last_id());
        Ok(poll(&bus, since, wait))
    }
}

/// The events after `since`, waiting up to `wait` (at most [`MAX_WAIT`]).
fn poll(bus: &EventBus, since: u64, wait: Duration) -> EventsDto {
    let (since, events, reset) = match bus.wait_after(since, wait.min(MAX_WAIT)) {
        Ok(events) => (since, events, None),
        Err(oldest) => {
            let since = oldest.saturating_sub(1);
            (since, bus.after(since).unwrap_or_default(), Some(oldest))
        }
    };
    EventsDto {
        last_id: events.last().map_or(since, |e| e.id),
        events: events
            .into_iter()
            .map(|e| EventDto {
                id: e.id,
                event: e.kind.to_owned(),
                data: e.data,
            })
            .collect(),
        reset,
    }
}

/// `GET /events?wait=`: the [`EventsDto`] after `since`, as one JSON body
/// that a thread of its own waits for, so the listener serves other
/// requests meanwhile.
#[must_use]
pub fn poll_response(bus: Arc<EventBus>, since: u64, wait: Duration) -> Response {
    let shared = Arc::new(Mutex::new(Shared::default()));
    let feed = Arc::clone(&shared);
    let spawned = std::thread::Builder::new()
        .name("fsonos-events-poll".into())
        .spawn(move || {
            let body = serde_json::to_vec(&poll(&bus, since, wait)).unwrap_or_default();
            let mut s = feed.lock().unwrap_or_else(PoisonError::into_inner);
            s.queue.push_back(body);
            s.finished = true;
            if let Some(waker) = s.waker.take() {
                waker.wake();
            }
        });
    if spawned.is_err() {
        shared
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .finished = true;
    }
    Response::ok()
        .header("content-type", b"application/json".to_vec())
        .header("cache-control", b"no-cache".to_vec())
        .body(ResponseBody::stream(EventStream { shared }))
}

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
        // Past the newest too: an id from before the daemon restarted.
        if after + 1 < oldest || after > self.last {
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
                if let Some(length) = changes.queue_length {
                    data.insert("queue_length".into(), json!(length));
                }
                if let Some(mode) = changes
                    .play_mode
                    .as_deref()
                    .and_then(fsonos_types::PlayMode::from_wire)
                {
                    let (shuffle, repeat) = crate::zones::shuffle_and_repeat(mode);
                    data.insert("shuffle".into(), json!(shuffle));
                    data.insert("repeat".into(), json!(repeat));
                    data.insert("play_mode".into(), json!(mode.as_str()));
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
    fn an_id_from_before_a_restart_is_a_gap() {
        let bus = EventBus::new();
        assert_eq!(bus.after(5000), Err(1), "nothing yet: start over from 1");
        publish(&bus, 3);
        assert_eq!(bus.after(5000), Err(1));
        let answer = poll(&bus, 5000, Duration::ZERO);
        assert_eq!(answer.reset, Some(1));
        assert_eq!(answer.events.len(), 3);
        assert_eq!(answer.last_id, 3);
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

    #[test]
    fn a_play_mode_change_says_shuffle_and_repeat() {
        let bus = EventBus::new();
        let households = crate::zones::fixtures::households();
        let player = crate::zones::fixtures::id("RINCON_DEN");
        let changes = fsonos_core::playback::Changes {
            play_mode: Some("REPEAT_ALL".into()),
            ..Default::default()
        };
        bus.publish_live(&LiveEvent::Playback { player, changes }, &households);
        let event = &bus.after(0).unwrap()[0];
        assert_eq!(event.data["shuffle"], json!(false), "{}", event.data);
        assert_eq!(event.data["repeat"], "all", "{}", event.data);
        assert_eq!(event.data["play_mode"], "REPEAT_ALL", "{}", event.data);
        assert!(event.data.get("volume").is_none());
    }
}
