//! Event store for SSE resumability.
//!
//! This module provides an [`EventStore`] that enables SSE polling and
//! resumability by storing events that can be replayed when clients reconnect.
//!
//! # SSE Resumability
//!
//! When a client disconnects from an SSE stream, it may miss events. The
//! `Last-Event-ID` header allows clients to indicate where they left off,
//! and the server can replay missed events using the EventStore.
//!
//! # Features
//!
//! - **TTL-based event retention**: Events automatically expire after a configurable duration
//! - **Per-stream event limits**: Prevents unbounded memory growth
//! - **Global retention limits**: Bounds streams, identifier bytes, and retained payload bytes
//! - **Cursor-based resumption**: Replay events from any point using event IDs
//! - **Bounded replay pages**: Stream-local resumption with explicit event and payload caps
//! - **Thread-safe**: Safe for concurrent access from multiple handlers
//!
//! # Example
//!
//! ```
//! use fastmcp_transport::event_store::{EventStore, EventStoreConfig};
//! use std::time::Duration;
//!
//! // Create event store with custom configuration
//! let store = EventStore::with_config(EventStoreConfig {
//!     max_events_per_stream: 100,
//!     ttl: Some(Duration::from_secs(3600)), // 1 hour
//!     ..EventStoreConfig::default()
//! })
//! .expect("valid event-store configuration");
//!
//! // Store an event
//! let stream_id = "session-123";
//! let event_id = store
//!     .store_event(stream_id, Some(serde_json::json!({"method": "test"})))
//!     .expect("event fits configured retention limits");
//!
//! // Replay one bounded page and retain its cursor for the next request.
//! let page = store
//!     .replay_bounded(stream_id, None)
//!     .expect("fresh stream replay is admitted");
//! let events = page.events();
//! ```

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

/// Default maximum events per stream.
pub const DEFAULT_MAX_EVENTS_PER_STREAM: usize = 100;

/// Default maximum number of retained streams.
pub const DEFAULT_MAX_STREAMS: usize = 1_024;

/// Default maximum UTF-8 byte length of a retained stream identifier.
pub const DEFAULT_MAX_STREAM_ID_BYTES: usize = 256;

/// Default maximum compact-JSON payload size of one event (1 MiB).
pub const DEFAULT_MAX_EVENT_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Default maximum compact-JSON payload bytes retained across all streams (64 MiB).
pub const DEFAULT_MAX_TOTAL_EVENT_PAYLOAD_BYTES: usize = 64 * 1024 * 1024;

/// Default maximum events returned by one modern replay page.
pub const DEFAULT_MAX_REPLAY_EVENTS: usize = 64;

/// Default compact-JSON payload bytes returned by one modern replay page (1 MiB).
pub const DEFAULT_MAX_REPLAY_PAYLOAD_BYTES: usize = 1024 * 1024;

/// Default maximum UTF-8 byte length of an untrusted replay cursor.
pub const DEFAULT_MAX_REPLAY_CURSOR_BYTES: usize = 256;

/// Default TTL for events (1 hour).
pub const DEFAULT_TTL_SECS: u64 = 3600;

/// Unique identifier for an event.
pub type EventId = String;

/// Unique identifier for a stream (session).
pub type StreamId = String;

/// Errors produced by event-store configuration or admission checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventStoreError {
    /// A configured retention limit was zero.
    InvalidZeroLimit {
        /// Name of the invalid configuration field.
        field: &'static str,
    },
    /// The stream identifier exceeded its configured UTF-8 byte limit.
    StreamIdTooLong {
        /// Configured maximum number of bytes.
        max_bytes: usize,
    },
    /// The compact JSON encoding of one event exceeded its configured limit.
    EventPayloadTooLarge {
        /// Configured maximum number of bytes.
        max_bytes: usize,
    },
    /// A new stream could not be retained without exceeding the stream limit.
    StreamLimitReached {
        /// Configured maximum number of streams.
        max_streams: usize,
    },
    /// An event could not be retained without exceeding the aggregate payload limit.
    AggregatePayloadLimitExceeded {
        /// Configured maximum aggregate number of bytes.
        max_bytes: usize,
    },
    /// A replay cursor exceeded its configured UTF-8 byte limit.
    ReplayCursorTooLong {
        /// Configured maximum number of bytes.
        max_bytes: usize,
    },
    /// A supplied replay cursor is not retained by the named stream.
    ///
    /// This intentionally does not disclose whether the cursor belongs to a
    /// different stream or has expired/been evicted from this one.
    ReplayCursorNotRetained,
    /// One retained event cannot fit in an otherwise empty replay page.
    ReplayEventPayloadTooLarge {
        /// Configured maximum compact-JSON payload bytes per replay page.
        max_bytes: usize,
    },
    /// A JSON value could not be measured using its compact wire encoding.
    PayloadMeasurementFailed,
}

impl std::fmt::Display for EventStoreError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidZeroLimit { field } => {
                write!(
                    formatter,
                    "event-store limit `{field}` must be greater than zero"
                )
            }
            Self::StreamIdTooLong { max_bytes } => write!(
                formatter,
                "stream identifier exceeds the configured {max_bytes}-byte limit"
            ),
            Self::EventPayloadTooLarge { max_bytes } => write!(
                formatter,
                "event payload exceeds the configured {max_bytes}-byte limit"
            ),
            Self::StreamLimitReached { max_streams } => write!(
                formatter,
                "event store has reached its configured {max_streams}-stream limit"
            ),
            Self::AggregatePayloadLimitExceeded { max_bytes } => write!(
                formatter,
                "event store would exceed its configured {max_bytes}-byte aggregate payload limit"
            ),
            Self::ReplayCursorTooLong { max_bytes } => write!(
                formatter,
                "replay cursor exceeds the configured {max_bytes}-byte limit"
            ),
            Self::ReplayCursorNotRetained => {
                formatter.write_str("replay cursor is not retained for the requested stream")
            }
            Self::ReplayEventPayloadTooLarge { max_bytes } => write!(
                formatter,
                "replay event exceeds the configured {max_bytes}-byte replay page limit"
            ),
            Self::PayloadMeasurementFailed => {
                formatter.write_str("event payload size could not be measured")
            }
        }
    }
}

impl std::error::Error for EventStoreError {}

/// A stored event with metadata.
#[derive(Debug, Clone)]
pub struct EventEntry {
    /// Unique event identifier.
    pub id: EventId,
    /// Stream this event belongs to.
    pub stream_id: StreamId,
    /// Event data (None for priming events).
    pub data: Option<serde_json::Value>,
    /// When this event was stored.
    pub created_at: Instant,
}

impl EventEntry {
    /// Creates a new event entry.
    fn new(id: EventId, stream_id: StreamId, data: Option<serde_json::Value>) -> Self {
        Self {
            id,
            stream_id,
            data,
            created_at: Instant::now(),
        }
    }

    /// Returns true if this event has expired based on the given TTL.
    fn is_expired(&self, ttl: Option<Duration>) -> bool {
        match ttl {
            Some(ttl) => self.created_at.elapsed() > ttl,
            None => false,
        }
    }
}

/// One bounded, immutable page of retained events for a named stream.
///
/// A caller resumes by passing [`Self::next_after_id`] back to
/// [`EventStore::replay_bounded`]. The page is a retention snapshot: events
/// stored after the method releases the store lock are never appended to this
/// value implicitly.
#[derive(Debug, Clone)]
pub struct ReplayBatch {
    events: Vec<EventEntry>,
    next_after_id: Option<EventId>,
    payload_bytes: usize,
    complete: bool,
}

impl ReplayBatch {
    fn empty() -> Self {
        Self {
            events: Vec::new(),
            next_after_id: None,
            payload_bytes: 0,
            complete: true,
        }
    }

    /// Returns the retained events in chronological order.
    #[must_use]
    pub fn events(&self) -> &[EventEntry] {
        &self.events
    }

    /// Consumes the page and returns its retained events in chronological order.
    #[must_use]
    pub fn into_events(self) -> Vec<EventEntry> {
        self.events
    }

    /// Returns the exclusive cursor for the next replay page, when one exists.
    ///
    /// If no event was emitted, this preserves the admitted input cursor. A
    /// fresh empty stream therefore has no cursor until its producer records
    /// an event or an explicit priming event.
    #[must_use]
    pub fn next_after_id(&self) -> Option<&str> {
        self.next_after_id.as_deref()
    }

    /// Returns the compact-JSON payload bytes in this page.
    #[must_use]
    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    /// Returns whether this page reached the retained tail at its snapshot.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }
}

/// Configuration for the event store.
#[derive(Debug, Clone)]
pub struct EventStoreConfig {
    /// Maximum number of events to retain per stream.
    pub max_events_per_stream: usize,
    /// Maximum number of streams to retain.
    pub max_streams: usize,
    /// Maximum UTF-8 byte length of a retained stream identifier.
    pub max_stream_id_bytes: usize,
    /// Maximum compact-JSON payload bytes retained for one event.
    pub max_event_payload_bytes: usize,
    /// Maximum compact-JSON payload bytes retained across all streams.
    pub max_total_event_payload_bytes: usize,
    /// Maximum events returned by one modern replay page.
    pub max_replay_events: usize,
    /// Maximum compact-JSON payload bytes returned by one modern replay page.
    pub max_replay_payload_bytes: usize,
    /// Maximum UTF-8 byte length accepted for an untrusted replay cursor.
    pub max_replay_cursor_bytes: usize,
    /// Time-to-live for events. `None` means events never expire.
    pub ttl: Option<Duration>,
}

impl Default for EventStoreConfig {
    fn default() -> Self {
        Self {
            max_events_per_stream: DEFAULT_MAX_EVENTS_PER_STREAM,
            max_streams: DEFAULT_MAX_STREAMS,
            max_stream_id_bytes: DEFAULT_MAX_STREAM_ID_BYTES,
            max_event_payload_bytes: DEFAULT_MAX_EVENT_PAYLOAD_BYTES,
            max_total_event_payload_bytes: DEFAULT_MAX_TOTAL_EVENT_PAYLOAD_BYTES,
            max_replay_events: DEFAULT_MAX_REPLAY_EVENTS,
            max_replay_payload_bytes: DEFAULT_MAX_REPLAY_PAYLOAD_BYTES,
            max_replay_cursor_bytes: DEFAULT_MAX_REPLAY_CURSOR_BYTES,
            ttl: Some(Duration::from_secs(DEFAULT_TTL_SECS)),
        }
    }
}

impl EventStoreConfig {
    /// Creates a config with no TTL (events never expire).
    #[must_use]
    pub fn no_expiry() -> Self {
        Self {
            ttl: None,
            ..Default::default()
        }
    }

    /// Sets the maximum events per stream.
    #[must_use]
    pub fn max_events(mut self, max: usize) -> Self {
        self.max_events_per_stream = max;
        self
    }

    /// Sets the maximum number of retained streams.
    #[must_use]
    pub fn max_streams(mut self, max: usize) -> Self {
        self.max_streams = max;
        self
    }

    /// Sets the maximum UTF-8 byte length of retained stream identifiers.
    #[must_use]
    pub fn max_stream_id_bytes(mut self, max: usize) -> Self {
        self.max_stream_id_bytes = max;
        self
    }

    /// Sets the maximum compact-JSON payload bytes retained for one event.
    #[must_use]
    pub fn max_event_payload_bytes(mut self, max: usize) -> Self {
        self.max_event_payload_bytes = max;
        self
    }

    /// Sets the maximum compact-JSON payload bytes retained across all streams.
    #[must_use]
    pub fn max_total_event_payload_bytes(mut self, max: usize) -> Self {
        self.max_total_event_payload_bytes = max;
        self
    }

    /// Sets the maximum events returned by one modern replay page.
    #[must_use]
    pub fn max_replay_events(mut self, max: usize) -> Self {
        self.max_replay_events = max;
        self
    }

    /// Sets the maximum compact-JSON payload bytes returned by one replay page.
    #[must_use]
    pub fn max_replay_payload_bytes(mut self, max: usize) -> Self {
        self.max_replay_payload_bytes = max;
        self
    }

    /// Sets the maximum UTF-8 byte length accepted for one replay cursor.
    #[must_use]
    pub fn max_replay_cursor_bytes(mut self, max: usize) -> Self {
        self.max_replay_cursor_bytes = max;
        self
    }

    /// Sets the TTL for events.
    #[must_use]
    pub fn ttl(mut self, ttl: Duration) -> Self {
        self.ttl = Some(ttl);
        self
    }

    /// Disables TTL (events never expire).
    #[must_use]
    pub fn no_ttl(mut self) -> Self {
        self.ttl = None;
        self
    }

    fn validate(&self) -> Result<(), EventStoreError> {
        for (field, limit) in [
            ("max_events_per_stream", self.max_events_per_stream),
            ("max_streams", self.max_streams),
            ("max_stream_id_bytes", self.max_stream_id_bytes),
            ("max_event_payload_bytes", self.max_event_payload_bytes),
            (
                "max_total_event_payload_bytes",
                self.max_total_event_payload_bytes,
            ),
            ("max_replay_events", self.max_replay_events),
            ("max_replay_payload_bytes", self.max_replay_payload_bytes),
            ("max_replay_cursor_bytes", self.max_replay_cursor_bytes),
        ] {
            if limit == 0 {
                return Err(EventStoreError::InvalidZeroLimit { field });
            }
        }

        Ok(())
    }
}

struct PayloadByteCounter {
    bytes: usize,
    max_bytes: usize,
    exceeded: bool,
}

impl PayloadByteCounter {
    fn new(max_bytes: usize) -> Self {
        Self {
            bytes: 0,
            max_bytes,
            exceeded: false,
        }
    }
}

impl std::io::Write for PayloadByteCounter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let Some(next_bytes) = self.bytes.checked_add(buffer.len()) else {
            self.exceeded = true;
            return Err(std::io::Error::other("event payload size overflow"));
        };

        if next_bytes > self.max_bytes {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "event payload exceeds configured limit",
            ));
        }

        self.bytes = next_bytes;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn measure_event_payload(
    data: Option<&serde_json::Value>,
    max_bytes: usize,
) -> Result<usize, EventStoreError> {
    let Some(data) = data else {
        return Ok(0);
    };

    let mut counter = PayloadByteCounter::new(max_bytes);
    match serde_json::to_writer(&mut counter, data) {
        Ok(()) => Ok(counter.bytes),
        Err(_) if counter.exceeded => Err(EventStoreError::EventPayloadTooLarge { max_bytes }),
        Err(_) => Err(EventStoreError::PayloadMeasurementFailed),
    }
}

/// Internal event representation with precomputed retention accounting.
#[derive(Debug)]
struct StoredEvent {
    entry: EventEntry,
    payload_bytes: usize,
}

/// Internal storage for a single stream's events.
#[derive(Debug)]
struct StreamEvents {
    /// Events in insertion order (oldest first).
    events: VecDeque<StoredEvent>,
    /// Map from event ID to index for fast lookup.
    index: HashMap<EventId, usize>,
}

impl StreamEvents {
    fn new() -> Self {
        Self {
            events: VecDeque::new(),
            index: HashMap::new(),
        }
    }

    /// Returns the payload bytes that would be evicted by one insertion.
    fn prospective_evicted_payload_bytes(&self, max_events: usize) -> Option<usize> {
        let remove_count = self.events.len().checked_add(1)?.saturating_sub(max_events);
        self.events
            .iter()
            .take(remove_count)
            .try_fold(0usize, |total, event| {
                total.checked_add(event.payload_bytes)
            })
    }

    /// Adds an event, enforcing the per-stream event limit.
    fn push(
        &mut self,
        entry: EventEntry,
        payload_bytes: usize,
        max_events: usize,
    ) -> Result<(), EventStoreError> {
        if max_events == 0 {
            return Err(EventStoreError::InvalidZeroLimit {
                field: "max_events_per_stream",
            });
        }

        // Remove oldest if at capacity
        while self.events.len() >= max_events {
            if let Some(oldest) = self.events.pop_front() {
                self.index.remove(&oldest.entry.id);
            }
            // Rebuild index after removal (indices shifted)
            self.rebuild_index();
        }

        let idx = self.events.len();
        self.index.insert(entry.id.clone(), idx);
        self.events.push_back(StoredEvent {
            entry,
            payload_bytes,
        });
        Ok(())
    }

    /// Removes expired events.
    fn remove_expired(&mut self, ttl: Option<Duration>) {
        if ttl.is_none() {
            return;
        }

        let mut removed = false;
        while let Some(front) = self.events.front() {
            if front.entry.is_expired(ttl) {
                if let Some(entry) = self.events.pop_front() {
                    self.index.remove(&entry.entry.id);
                    removed = true;
                }
            } else {
                break;
            }
        }

        if removed {
            self.rebuild_index();
        }
    }

    /// Rebuilds the index after removals.
    fn rebuild_index(&mut self) {
        self.index.clear();
        for (idx, entry) in self.events.iter().enumerate() {
            self.index.insert(entry.entry.id.clone(), idx);
        }
    }

    fn total_payload_bytes(&self) -> Option<usize> {
        self.events.iter().try_fold(0usize, |total, event| {
            total.checked_add(event.payload_bytes)
        })
    }

    /// Gets events after the specified ID (exclusive).
    fn events_after(&self, after_id: Option<&str>) -> Vec<EventEntry> {
        match after_id {
            None => self
                .events
                .iter()
                .map(|event| event.entry.clone())
                .collect(),
            Some(id) => {
                if let Some(&idx) = self.index.get(id) {
                    self.events
                        .iter()
                        .skip(idx + 1)
                        .map(|event| event.entry.clone())
                        .collect()
                } else {
                    // ID not found, return empty (client should reconnect fresh)
                    Vec::new()
                }
            }
        }
    }

    /// Returns one bounded, stream-local page after an exclusive retained cursor.
    fn replay_bounded(
        &self,
        after_id: Option<&str>,
        max_events: usize,
        max_payload_bytes: usize,
    ) -> Result<ReplayBatch, EventStoreError> {
        let start_index = match after_id {
            Some(id) => self
                .index
                .get(id)
                .map(|index| index.saturating_add(1))
                .ok_or(EventStoreError::ReplayCursorNotRetained)?,
            None => 0,
        };

        let available = self.events.len().saturating_sub(start_index);
        let mut events = Vec::with_capacity(available.min(max_events));
        let mut next_after_id = after_id.map(str::to_owned);
        let mut payload_bytes = 0;

        for event in self.events.iter().skip(start_index) {
            if events.len() == max_events {
                return Ok(ReplayBatch {
                    events,
                    next_after_id,
                    payload_bytes,
                    complete: false,
                });
            }

            let next_payload_bytes = payload_bytes.checked_add(event.payload_bytes).ok_or(
                EventStoreError::ReplayEventPayloadTooLarge {
                    max_bytes: max_payload_bytes,
                },
            )?;
            if next_payload_bytes > max_payload_bytes {
                if events.is_empty() {
                    return Err(EventStoreError::ReplayEventPayloadTooLarge {
                        max_bytes: max_payload_bytes,
                    });
                }

                return Ok(ReplayBatch {
                    events,
                    next_after_id,
                    payload_bytes,
                    complete: false,
                });
            }

            payload_bytes = next_payload_bytes;
            next_after_id = Some(event.entry.id.clone());
            events.push(event.entry.clone());
        }

        Ok(ReplayBatch {
            events,
            next_after_id,
            payload_bytes,
            complete: true,
        })
    }

    /// Finds the stream ID for a given event ID.
    fn contains(&self, event_id: &str) -> bool {
        self.index.contains_key(event_id)
    }
}

/// Thread-safe event store for SSE resumability.
///
/// Stores events per stream with automatic expiration and size limits.
/// Use this to enable clients to resume SSE streams after disconnection.
///
/// # Thread Safety
///
/// The EventStore uses `RwLock` internally and is safe for concurrent
/// access from multiple threads.
#[derive(Debug)]
pub struct EventStore {
    /// Configuration.
    config: EventStoreConfig,
    /// Per-stream event storage.
    streams: RwLock<HashMap<StreamId, StreamEvents>>,
    /// Counter for generating unique event IDs.
    event_counter: AtomicU64,
}

impl Default for EventStore {
    fn default() -> Self {
        Self::new()
    }
}

impl EventStore {
    /// Creates a new event store with default configuration.
    #[must_use]
    pub fn new() -> Self {
        Self::from_validated_config(EventStoreConfig::default())
    }

    /// Creates a new event store with custom configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if any configured retention limit is zero.
    pub fn with_config(config: EventStoreConfig) -> Result<Self, EventStoreError> {
        config.validate()?;
        Ok(Self::from_validated_config(config))
    }

    fn from_validated_config(config: EventStoreConfig) -> Self {
        Self {
            config,
            streams: RwLock::new(HashMap::new()),
            event_counter: AtomicU64::new(0),
        }
    }

    /// Returns the configuration.
    #[must_use]
    pub fn config(&self) -> &EventStoreConfig {
        &self.config
    }

    /// Generates a unique event ID.
    fn generate_event_id(&self) -> EventId {
        let counter = self.event_counter.fetch_add(1, Ordering::Relaxed);
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        format!("{timestamp}-{counter}")
    }

    fn cleanup_streams(streams: &mut HashMap<StreamId, StreamEvents>, ttl: Option<Duration>) {
        if ttl.is_some() {
            for stream in streams.values_mut() {
                stream.remove_expired(ttl);
            }
        }
        streams.retain(|_, stream| !stream.events.is_empty());
    }

    fn total_payload_bytes(streams: &HashMap<StreamId, StreamEvents>) -> Option<usize> {
        streams.values().try_fold(0usize, |total, stream| {
            total.checked_add(stream.total_payload_bytes()?)
        })
    }

    /// Stores an event and returns its ID.
    ///
    /// # Arguments
    ///
    /// * `stream_id` - The stream (session) this event belongs to
    /// * `data` - Event data, or `None` for a priming event
    ///
    /// # Returns
    ///
    /// The unique event ID that can be used for resumption.
    ///
    /// # Errors
    ///
    /// Returns an error without retaining the event if an input or aggregate
    /// retention limit would be exceeded.
    pub fn store_event(
        &self,
        stream_id: &str,
        data: Option<serde_json::Value>,
    ) -> Result<EventId, EventStoreError> {
        if stream_id.len() > self.config.max_stream_id_bytes {
            return Err(EventStoreError::StreamIdTooLong {
                max_bytes: self.config.max_stream_id_bytes,
            });
        }

        let payload_bytes =
            measure_event_payload(data.as_ref(), self.config.max_event_payload_bytes)?;

        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        Self::cleanup_streams(&mut streams, self.config.ttl);

        if !streams.contains_key(stream_id) && streams.len() >= self.config.max_streams {
            return Err(EventStoreError::StreamLimitReached {
                max_streams: self.config.max_streams,
            });
        }

        let total_payload_bytes = Self::total_payload_bytes(&streams).ok_or(
            EventStoreError::AggregatePayloadLimitExceeded {
                max_bytes: self.config.max_total_event_payload_bytes,
            },
        )?;
        let evicted_payload_bytes = streams
            .get(stream_id)
            .map_or(Some(0), |stream| {
                stream.prospective_evicted_payload_bytes(self.config.max_events_per_stream)
            })
            .ok_or(EventStoreError::AggregatePayloadLimitExceeded {
                max_bytes: self.config.max_total_event_payload_bytes,
            })?;
        let retained_payload_bytes = total_payload_bytes
            .checked_sub(evicted_payload_bytes)
            .and_then(|retained| retained.checked_add(payload_bytes))
            .ok_or(EventStoreError::AggregatePayloadLimitExceeded {
                max_bytes: self.config.max_total_event_payload_bytes,
            })?;

        if retained_payload_bytes > self.config.max_total_event_payload_bytes {
            return Err(EventStoreError::AggregatePayloadLimitExceeded {
                max_bytes: self.config.max_total_event_payload_bytes,
            });
        }

        let event_id = self.generate_event_id();
        let entry = EventEntry::new(event_id.clone(), stream_id.to_string(), data);

        let stream = streams
            .entry(stream_id.to_string())
            .or_insert_with(StreamEvents::new);

        stream.push(entry, payload_bytes, self.config.max_events_per_stream)?;

        Ok(event_id)
    }

    /// Stores a priming event (empty data) for SSE initialization.
    ///
    /// Per SSE spec, servers should send an event with just an ID to prime
    /// the client's `Last-Event-ID` tracking.
    ///
    /// # Errors
    ///
    /// Returns an error without retaining the event if the stream identifier
    /// or store-wide retention limits would be exceeded.
    pub fn store_priming_event(&self, stream_id: &str) -> Result<EventId, EventStoreError> {
        self.store_event(stream_id, None)
    }

    /// Gets events after the specified event ID.
    ///
    /// # Arguments
    ///
    /// * `stream_id` - The stream to get events from
    /// * `after_id` - Get events after this ID (exclusive). `None` returns all events.
    ///
    /// # Returns
    ///
    /// Vector of events in chronological order.
    #[must_use]
    pub fn get_events_after(&self, stream_id: &str, after_id: Option<&str>) -> Vec<EventEntry> {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        Self::cleanup_streams(&mut streams, self.config.ttl);

        if let Some(stream) = streams.get(stream_id) {
            stream.events_after(after_id)
        } else {
            Vec::new()
        }
    }

    /// Returns one bounded replay page for a single named stream.
    ///
    /// `after_id` is exclusive. A supplied cursor must still be retained by
    /// `stream_id`; this method never searches another stream and never falls
    /// back to the retained beginning after expiry or eviction. Pass the
    /// returned [`ReplayBatch::next_after_id`] to resume a partial page.
    ///
    /// The returned page is bounded by both
    /// [`EventStoreConfig::max_replay_events`] and
    /// [`EventStoreConfig::max_replay_payload_bytes`]. It is an immutable
    /// snapshot taken while the store lock is held, so callers can stream or
    /// otherwise consume it after this method returns without holding the
    /// store lock.
    ///
    /// This is a retention primitive only. HTTP and subscription consumers
    /// remain responsible for authenticating and authorizing the named stream,
    /// applying their own protocol framing, and deciding whether resumption is
    /// available for a request.
    ///
    /// # Errors
    ///
    /// Returns an error if the stream ID or cursor exceeds its configured
    /// bounds, the cursor is not retained by this stream, or the first eligible
    /// event cannot fit in an otherwise empty replay page.
    pub fn replay_bounded(
        &self,
        stream_id: &str,
        after_id: Option<&str>,
    ) -> Result<ReplayBatch, EventStoreError> {
        if stream_id.len() > self.config.max_stream_id_bytes {
            return Err(EventStoreError::StreamIdTooLong {
                max_bytes: self.config.max_stream_id_bytes,
            });
        }
        if after_id.is_some_and(|id| id.len() > self.config.max_replay_cursor_bytes) {
            return Err(EventStoreError::ReplayCursorTooLong {
                max_bytes: self.config.max_replay_cursor_bytes,
            });
        }

        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        Self::cleanup_streams(&mut streams, self.config.ttl);

        match streams.get(stream_id) {
            Some(stream) => stream.replay_bounded(
                after_id,
                self.config.max_replay_events,
                self.config.max_replay_payload_bytes,
            ),
            None if after_id.is_some() => Err(EventStoreError::ReplayCursorNotRetained),
            None => Ok(ReplayBatch::empty()),
        }
    }

    /// Replays events after a specific event ID using a callback.
    ///
    /// This is the primary method for SSE resumption. When a client reconnects
    /// with a `Last-Event-ID`, use this to replay missed events.
    ///
    /// # Arguments
    ///
    /// * `last_event_id` - The client's last received event ID
    /// * `callback` - Called for each event to replay
    ///
    /// # Returns
    ///
    /// The stream ID if the event was found, `None` otherwise.
    pub fn replay_events_after<F>(&self, last_event_id: &str, mut callback: F) -> Option<StreamId>
    where
        F: FnMut(&EventEntry),
    {
        let replay = {
            let mut streams = self
                .streams
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);

            Self::cleanup_streams(&mut streams, self.config.ttl);
            streams.iter().find_map(|(stream_id, stream)| {
                stream
                    .contains(last_event_id)
                    .then(|| (stream_id.clone(), stream.events_after(Some(last_event_id))))
            })
        };

        let (stream_id, events) = replay?;
        for event in events {
            callback(&event);
        }
        Some(stream_id)
    }

    /// Looks up the stream ID for a given event ID.
    ///
    /// # Returns
    ///
    /// The stream ID if the event exists, `None` otherwise.
    #[must_use]
    pub fn find_stream_for_event(&self, event_id: &str) -> Option<StreamId> {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);

        Self::cleanup_streams(&mut streams, self.config.ttl);

        for (stream_id, stream) in streams.iter() {
            if stream.contains(event_id) {
                return Some(stream_id.clone());
            }
        }

        None
    }

    /// Removes all events for a stream.
    ///
    /// Call this when a session ends to free memory.
    pub fn clear_stream(&self, stream_id: &str) {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        streams.remove(stream_id);
    }

    /// Removes all expired events across all streams.
    ///
    /// This is called automatically during operations, but you can call
    /// it manually for cleanup.
    pub fn cleanup_expired(&self) {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::cleanup_streams(&mut streams, self.config.ttl);
    }

    /// Returns the number of streams currently stored.
    #[must_use]
    pub fn stream_count(&self) -> usize {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::cleanup_streams(&mut streams, self.config.ttl);
        streams.len()
    }

    /// Returns the total number of events across all streams.
    #[must_use]
    pub fn event_count(&self) -> usize {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::cleanup_streams(&mut streams, self.config.ttl);
        streams.values().map(|s| s.events.len()).sum()
    }

    /// Returns statistics about the event store.
    #[must_use]
    pub fn stats(&self) -> EventStoreStats {
        let mut streams = self
            .streams
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Self::cleanup_streams(&mut streams, self.config.ttl);
        let total_events: usize = streams.values().map(|s| s.events.len()).sum();
        let total_event_payload_bytes = Self::total_payload_bytes(&streams)
            .unwrap_or(self.config.max_total_event_payload_bytes);

        EventStoreStats {
            stream_count: streams.len(),
            total_events,
            total_event_payload_bytes,
            max_events_per_stream: self.config.max_events_per_stream,
            max_streams: self.config.max_streams,
            max_stream_id_bytes: self.config.max_stream_id_bytes,
            max_event_payload_bytes: self.config.max_event_payload_bytes,
            max_total_event_payload_bytes: self.config.max_total_event_payload_bytes,
            max_replay_events: self.config.max_replay_events,
            max_replay_payload_bytes: self.config.max_replay_payload_bytes,
            max_replay_cursor_bytes: self.config.max_replay_cursor_bytes,
            ttl: self.config.ttl,
        }
    }
}

/// Statistics about the event store.
#[derive(Debug, Clone)]
pub struct EventStoreStats {
    /// Number of streams.
    pub stream_count: usize,
    /// Total events across all streams.
    pub total_events: usize,
    /// Compact-JSON payload bytes retained across all streams.
    pub total_event_payload_bytes: usize,
    /// Configured max events per stream.
    pub max_events_per_stream: usize,
    /// Configured maximum number of retained streams.
    pub max_streams: usize,
    /// Configured maximum stream identifier byte length.
    pub max_stream_id_bytes: usize,
    /// Configured maximum compact-JSON bytes for one event payload.
    pub max_event_payload_bytes: usize,
    /// Configured maximum compact-JSON bytes retained across all events.
    pub max_total_event_payload_bytes: usize,
    /// Configured maximum events returned by one replay page.
    pub max_replay_events: usize,
    /// Configured maximum compact-JSON bytes returned by one replay page.
    pub max_replay_payload_bytes: usize,
    /// Configured maximum replay cursor byte length.
    pub max_replay_cursor_bytes: usize,
    /// Configured TTL.
    pub ttl: Option<Duration>,
}

/// A shared event store for use across multiple handlers.
pub type SharedEventStore = Arc<EventStore>;

/// Creates a shared event store with default configuration.
#[must_use]
pub fn create_shared_event_store() -> SharedEventStore {
    Arc::new(EventStore::new())
}

/// Creates a shared event store with custom configuration.
///
/// # Errors
///
/// Returns an error if any configured retention limit is zero.
pub fn create_shared_event_store_with_config(
    config: EventStoreConfig,
) -> Result<SharedEventStore, EventStoreError> {
    EventStore::with_config(config).map(Arc::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_store_and_retrieve_event() {
        let store = EventStore::new();

        let event_id = store
            .store_event("stream1", Some(serde_json::json!({"test": true})))
            .unwrap();
        assert_ne!(event_id.len(), 0);

        let events = store.get_events_after("stream1", None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, event_id);
        assert!(events[0].data.is_some());
    }

    #[test]
    fn test_store_priming_event() {
        let store = EventStore::new();

        let event_id = store.store_priming_event("stream1").unwrap();
        assert_ne!(event_id.len(), 0);

        let events = store.get_events_after("stream1", None);
        assert_eq!(events.len(), 1);
        assert!(events[0].data.is_none());
    }

    #[test]
    fn test_events_after_id() {
        let store = EventStore::new();

        let id1 = store
            .store_event("stream1", Some(serde_json::json!({"n": 1})))
            .unwrap();
        let id2 = store
            .store_event("stream1", Some(serde_json::json!({"n": 2})))
            .unwrap();
        let id3 = store
            .store_event("stream1", Some(serde_json::json!({"n": 3})))
            .unwrap();

        // Get events after id1 (should return id2 and id3)
        let events = store.get_events_after("stream1", Some(&id1));
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, id2);
        assert_eq!(events[1].id, id3);

        // Get events after id2 (should return id3)
        let events = store.get_events_after("stream1", Some(&id2));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].id, id3);

        // Get events after id3 (should return nothing)
        let events = store.get_events_after("stream1", Some(&id3));
        assert!(events.is_empty());
    }

    fn bounded_replay_fixture() -> (EventStore, EventId, EventId, EventId, EventId) {
        let store = EventStore::with_config(
            EventStoreConfig::no_expiry()
                .max_events(3)
                .max_replay_events(2)
                .max_replay_payload_bytes(2),
        )
        .unwrap();

        let id1 = store
            .store_event("subscription", Some(serde_json::json!(1)))
            .unwrap();
        let id2 = store
            .store_event("subscription", Some(serde_json::json!(2)))
            .unwrap();
        let id3 = store
            .store_event("subscription", Some(serde_json::json!(3)))
            .unwrap();
        let id4 = store
            .store_event("subscription", Some(serde_json::json!(4)))
            .unwrap();

        (store, id1, id2, id3, id4)
    }

    #[test]
    fn bounded_replay_resumes_from_a_retained_stream_cursor() {
        let (store, _evicted_id, retained_id, id3, id4) = bounded_replay_fixture();

        let batch = store
            .replay_bounded("subscription", Some(&retained_id))
            .expect("retained stream cursor must resume the later events");

        assert_eq!(
            batch
                .events()
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            vec![id3.as_str(), id4.as_str()]
        );
        assert_eq!(batch.payload_bytes(), 2);
        assert_eq!(batch.next_after_id(), Some(id4.as_str()));
        assert!(batch.is_complete());
    }

    #[test]
    fn bounded_replay_rejects_an_evicted_cursor_without_mutating_retained_state() {
        let (store, evicted_id, retained_id, id3, id4) = bounded_replay_fixture();
        let before_event_ids = store
            .get_events_after("subscription", None)
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();
        let before_stats = store.stats();

        let error = store
            .replay_bounded("subscription", Some(&evicted_id))
            .expect_err("an evicted cursor must never fall back to the retained beginning");

        assert_eq!(error, EventStoreError::ReplayCursorNotRetained);
        assert_eq!(
            store
                .get_events_after("subscription", None)
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            before_event_ids
        );
        assert_eq!(store.stats().total_events, before_stats.total_events);
        assert_eq!(
            store.stats().total_event_payload_bytes,
            before_stats.total_event_payload_bytes
        );

        let retained = store
            .replay_bounded("subscription", Some(&retained_id))
            .expect("only the cursor differs from the admitted retained resumption");
        assert_eq!(
            retained
                .into_events()
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![id3, id4]
        );
    }

    #[test]
    fn bounded_replay_rejects_a_cursor_from_another_stream_without_mutation() {
        let store = EventStore::with_config(EventStoreConfig::no_expiry()).unwrap();
        let target_cursor = store
            .store_event("target", Some(serde_json::json!(1)))
            .unwrap();
        let target_tail = store
            .store_event("target", Some(serde_json::json!(2)))
            .unwrap();
        let foreign_cursor = store
            .store_event("other", Some(serde_json::json!(3)))
            .unwrap();
        let before_event_ids = store
            .get_events_after("target", None)
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();

        let error = store
            .replay_bounded("target", Some(&foreign_cursor))
            .expect_err("a stream-local replay must reject a cursor from another stream");

        assert_eq!(error, EventStoreError::ReplayCursorNotRetained);
        assert_eq!(
            store
                .get_events_after("target", None)
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            before_event_ids
        );

        let resumed = store
            .replay_bounded("target", Some(&target_cursor))
            .expect("the admitted cursor differs only by belonging to the requested stream");
        assert_eq!(
            resumed
                .into_events()
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![target_tail]
        );
    }

    #[test]
    fn bounded_replay_pages_a_snapshot_without_skipping_the_tail() {
        let store = EventStore::with_config(
            EventStoreConfig::no_expiry()
                .max_replay_events(3)
                .max_replay_payload_bytes(2),
        )
        .unwrap();
        let id1 = store
            .store_event("subscription", Some(serde_json::json!(1)))
            .unwrap();
        let id2 = store
            .store_event("subscription", Some(serde_json::json!(2)))
            .unwrap();
        let id3 = store
            .store_event("subscription", Some(serde_json::json!(3)))
            .unwrap();

        let first = store
            .replay_bounded("subscription", None)
            .expect("initial bounded page must be admitted");
        assert_eq!(
            first
                .events()
                .iter()
                .map(|event| event.id.as_str())
                .collect::<Vec<_>>(),
            vec![id1.as_str(), id2.as_str()]
        );
        assert_eq!(first.payload_bytes(), 2);
        assert!(!first.is_complete());

        let second = store
            .replay_bounded("subscription", first.next_after_id())
            .expect("the page cursor must resume at the first unreturned event");
        assert!(second.is_complete());
        assert_eq!(
            second
                .into_events()
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            vec![id3]
        );
    }

    #[test]
    fn bounded_replay_rejects_an_oversize_first_event_without_skipping_it() {
        let store = EventStore::with_config(
            EventStoreConfig::no_expiry()
                .max_event_payload_bytes(2)
                .max_replay_payload_bytes(1),
        )
        .unwrap();
        let event_id = store
            .store_event("subscription", Some(serde_json::json!(42)))
            .unwrap();
        let before_event_ids = store
            .get_events_after("subscription", None)
            .into_iter()
            .map(|event| event.id)
            .collect::<Vec<_>>();

        let error = store
            .replay_bounded("subscription", None)
            .expect_err("an oversize first event must not be skipped to make a page fit");

        assert_eq!(
            error,
            EventStoreError::ReplayEventPayloadTooLarge { max_bytes: 1 }
        );
        assert_eq!(
            store
                .get_events_after("subscription", None)
                .into_iter()
                .map(|event| event.id)
                .collect::<Vec<_>>(),
            before_event_ids
        );
        assert_eq!(before_event_ids, vec![event_id]);
    }

    #[test]
    fn test_multiple_streams() {
        let store = EventStore::new();

        let id1 = store
            .store_event("stream1", Some(serde_json::json!({"stream": 1})))
            .unwrap();
        let id2 = store
            .store_event("stream2", Some(serde_json::json!({"stream": 2})))
            .unwrap();

        let events1 = store.get_events_after("stream1", None);
        let events2 = store.get_events_after("stream2", None);

        assert_eq!(events1.len(), 1);
        assert_eq!(events1[0].id, id1);

        assert_eq!(events2.len(), 1);
        assert_eq!(events2[0].id, id2);
    }

    #[test]
    fn test_max_events_limit() {
        let config = EventStoreConfig::default().max_events(3);
        let store = EventStore::with_config(config).unwrap();

        let _id1 = store
            .store_event("stream1", Some(serde_json::json!({"n": 1})))
            .unwrap();
        let _id2 = store
            .store_event("stream1", Some(serde_json::json!({"n": 2})))
            .unwrap();
        let id3 = store
            .store_event("stream1", Some(serde_json::json!({"n": 3})))
            .unwrap();
        let id4 = store
            .store_event("stream1", Some(serde_json::json!({"n": 4})))
            .unwrap();

        // Should only have 3 events (oldest removed)
        let events = store.get_events_after("stream1", None);
        assert_eq!(events.len(), 3);

        // First event should be id2 (id1 was evicted)
        // Actually first should be id2 since we push id4 after id3
        // With max 3: after adding id4, we have id2, id3, id4
        assert_eq!(events[1].id, id3);
        assert_eq!(events[2].id, id4);
    }

    #[test]
    fn test_replay_events() {
        let store = EventStore::new();

        let id1 = store
            .store_event("stream1", Some(serde_json::json!({"n": 1})))
            .unwrap();
        let id2 = store
            .store_event("stream1", Some(serde_json::json!({"n": 2})))
            .unwrap();
        let id3 = store
            .store_event("stream1", Some(serde_json::json!({"n": 3})))
            .unwrap();

        let mut replayed = Vec::new();
        let stream_id = store.replay_events_after(&id1, |event| {
            replayed.push(event.id.clone());
        });

        assert_eq!(stream_id, Some("stream1".to_string()));
        assert_eq!(replayed, vec![id2, id3]);
    }

    #[test]
    fn test_replay_unknown_event_id() {
        let store = EventStore::new();
        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();

        let mut replayed = Vec::new();
        let stream_id = store.replay_events_after("nonexistent", |event| {
            replayed.push(event.id.clone());
        });

        assert!(stream_id.is_none());
        assert_eq!(replayed.len(), 0);
    }

    #[test]
    fn test_find_stream_for_event() {
        let store = EventStore::new();

        let id1 = store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        let id2 = store
            .store_event("stream2", Some(serde_json::json!({})))
            .unwrap();

        assert_eq!(
            store.find_stream_for_event(&id1),
            Some("stream1".to_string())
        );
        assert_eq!(
            store.find_stream_for_event(&id2),
            Some("stream2".to_string())
        );
        assert_eq!(store.find_stream_for_event("nonexistent"), None);
    }

    #[test]
    fn test_clear_stream() {
        let store = EventStore::new();

        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        store
            .store_event("stream2", Some(serde_json::json!({})))
            .unwrap();

        assert_eq!(store.stream_count(), 2);

        store.clear_stream("stream1");

        assert_eq!(store.stream_count(), 1);
        assert!(store.get_events_after("stream1", None).is_empty());
    }

    #[test]
    fn test_event_expiration() {
        let config = EventStoreConfig {
            max_events_per_stream: 100,
            ttl: Some(Duration::from_millis(10)),
            ..EventStoreConfig::default()
        };
        let store = EventStore::with_config(config).unwrap();

        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();

        // Events should exist initially
        assert_eq!(store.get_events_after("stream1", None).len(), 1);

        // Wait for expiration
        std::thread::sleep(Duration::from_millis(20));

        // Events should be gone after cleanup
        store.cleanup_expired();
        assert!(store.get_events_after("stream1", None).is_empty());
    }

    #[test]
    fn test_no_expiration() {
        let config = EventStoreConfig::no_expiry();
        let store = EventStore::with_config(config).unwrap();

        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();

        // Even after cleanup, events should remain
        store.cleanup_expired();
        assert_eq!(store.get_events_after("stream1", None).len(), 1);
    }

    #[test]
    fn test_stats() {
        let store = EventStore::new();

        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        store
            .store_event("stream2", Some(serde_json::json!({})))
            .unwrap();

        let stats = store.stats();
        assert_eq!(stats.stream_count, 2);
        assert_eq!(stats.total_events, 3);
        assert_eq!(stats.total_event_payload_bytes, 6);
    }

    #[test]
    fn test_shared_event_store() {
        let store = create_shared_event_store();

        // Clone for multiple "handlers"
        let store1 = Arc::clone(&store);
        let store2 = Arc::clone(&store);

        store1
            .store_event("stream1", Some(serde_json::json!({"from": 1})))
            .unwrap();
        store2
            .store_event("stream1", Some(serde_json::json!({"from": 2})))
            .unwrap();

        assert_eq!(store.event_count(), 2);
    }

    #[test]
    fn test_unique_event_ids() {
        let store = EventStore::new();

        let id1 = store.store_event("stream1", None).unwrap();
        let id2 = store.store_event("stream1", None).unwrap();
        let id3 = store.store_event("stream2", None).unwrap();

        // All IDs should be unique
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
        assert_ne!(id1, id3);
    }

    #[test]
    fn test_config_builder() {
        let config = EventStoreConfig::default()
            .max_events(50)
            .max_streams(25)
            .max_stream_id_bytes(128)
            .max_event_payload_bytes(4096)
            .max_total_event_payload_bytes(8192)
            .max_replay_events(12)
            .max_replay_payload_bytes(2048)
            .max_replay_cursor_bytes(64)
            .ttl(Duration::from_secs(300));

        assert_eq!(config.max_events_per_stream, 50);
        assert_eq!(config.max_streams, 25);
        assert_eq!(config.max_stream_id_bytes, 128);
        assert_eq!(config.max_event_payload_bytes, 4096);
        assert_eq!(config.max_total_event_payload_bytes, 8192);
        assert_eq!(config.max_replay_events, 12);
        assert_eq!(config.max_replay_payload_bytes, 2048);
        assert_eq!(config.max_replay_cursor_bytes, 64);
        assert_eq!(config.ttl, Some(Duration::from_secs(300)));

        let config = config.no_ttl();
        assert!(config.ttl.is_none());
    }

    #[test]
    fn zero_max_events_is_rejected_without_entering_retention_loop() {
        let error = EventStore::with_config(EventStoreConfig::default().max_events(0))
            .expect_err("zero max_events_per_stream must be rejected");

        assert_eq!(
            error,
            EventStoreError::InvalidZeroLimit {
                field: "max_events_per_stream"
            }
        );
    }

    #[test]
    fn all_other_zero_retention_limits_are_rejected() {
        let cases = [
            ("max_streams", EventStoreConfig::default().max_streams(0)),
            (
                "max_stream_id_bytes",
                EventStoreConfig::default().max_stream_id_bytes(0),
            ),
            (
                "max_event_payload_bytes",
                EventStoreConfig::default().max_event_payload_bytes(0),
            ),
            (
                "max_total_event_payload_bytes",
                EventStoreConfig::default().max_total_event_payload_bytes(0),
            ),
            (
                "max_replay_events",
                EventStoreConfig::default().max_replay_events(0),
            ),
            (
                "max_replay_payload_bytes",
                EventStoreConfig::default().max_replay_payload_bytes(0),
            ),
            (
                "max_replay_cursor_bytes",
                EventStoreConfig::default().max_replay_cursor_bytes(0),
            ),
        ];

        for (field, config) in cases {
            let error =
                EventStore::with_config(config).expect_err("zero retention limit must be rejected");
            assert_eq!(error, EventStoreError::InvalidZeroLimit { field });
        }
    }

    #[test]
    fn stream_id_byte_limit_accepts_exact_boundary_and_rejects_one_more() {
        let store = EventStore::with_config(
            EventStoreConfig::default()
                .max_streams(2)
                .max_stream_id_bytes(4),
        )
        .unwrap();

        store.store_priming_event("éé").unwrap();
        let error = store
            .store_priming_event("ééx")
            .expect_err("five-byte stream identifier must be rejected");

        assert_eq!(error, EventStoreError::StreamIdTooLong { max_bytes: 4 });
        assert_eq!(store.stream_count(), 1);
    }

    #[test]
    fn stream_limit_accepts_exact_boundary_and_rejects_one_more() {
        let store = EventStore::with_config(EventStoreConfig::default().max_streams(2)).unwrap();

        store.store_priming_event("one").unwrap();
        store.store_priming_event("two").unwrap();
        let error = store
            .store_priming_event("three")
            .expect_err("third retained stream must be rejected");

        assert_eq!(
            error,
            EventStoreError::StreamLimitReached { max_streams: 2 }
        );
        assert_eq!(store.stream_count(), 2);
        assert_eq!(store.event_count(), 2);
    }

    #[test]
    fn event_payload_limit_accepts_exact_boundary_and_rejects_one_more() {
        let store = EventStore::with_config(
            EventStoreConfig::default()
                .max_event_payload_bytes(4)
                .max_total_event_payload_bytes(8),
        )
        .unwrap();

        store
            .store_event("stream", Some(serde_json::json!(1234)))
            .unwrap();
        let error = store
            .store_event("stream", Some(serde_json::json!(12345)))
            .expect_err("five-byte compact JSON payload must be rejected");

        assert_eq!(
            error,
            EventStoreError::EventPayloadTooLarge { max_bytes: 4 }
        );
        assert_eq!(store.event_count(), 1);
        assert_eq!(store.stats().total_event_payload_bytes, 4);
    }

    #[test]
    fn aggregate_payload_limit_accepts_exact_boundary_and_rejects_one_more() {
        let store = EventStore::with_config(
            EventStoreConfig::default()
                .max_event_payload_bytes(4)
                .max_total_event_payload_bytes(8),
        )
        .unwrap();

        store
            .store_event("stream", Some(serde_json::json!(1234)))
            .unwrap();
        store
            .store_event("stream", Some(serde_json::json!(5678)))
            .unwrap();
        let error = store
            .store_event("stream", Some(serde_json::json!(0)))
            .expect_err("ninth retained payload byte must be rejected");

        assert_eq!(
            error,
            EventStoreError::AggregatePayloadLimitExceeded { max_bytes: 8 }
        );
        let stats = store.stats();
        assert_eq!(stats.total_events, 2);
        assert_eq!(stats.total_event_payload_bytes, 8);
    }

    #[test]
    fn per_stream_eviction_releases_aggregate_payload_capacity() {
        let store = EventStore::with_config(
            EventStoreConfig::default()
                .max_events(1)
                .max_event_payload_bytes(4)
                .max_total_event_payload_bytes(4),
        )
        .unwrap();

        store
            .store_event("stream", Some(serde_json::json!(1234)))
            .unwrap();
        store
            .store_event("stream", Some(serde_json::json!(5678)))
            .unwrap();

        let events = store.get_events_after("stream", None);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, Some(serde_json::json!(5678)));
        assert_eq!(store.stats().total_event_payload_bytes, 4);
    }

    #[test]
    fn store_admission_opportunistically_reclaims_expired_stream_and_payload() {
        let store = EventStore::with_config(
            EventStoreConfig::default()
                .max_streams(1)
                .max_event_payload_bytes(4)
                .max_total_event_payload_bytes(4)
                .ttl(Duration::from_millis(5)),
        )
        .unwrap();

        store
            .store_event("old", Some(serde_json::json!(1234)))
            .unwrap();
        std::thread::sleep(Duration::from_millis(10));
        store
            .store_event("new", Some(serde_json::json!(5678)))
            .unwrap();

        assert!(store.get_events_after("old", None).is_empty());
        assert_eq!(store.stream_count(), 1);
        assert_eq!(store.stats().total_event_payload_bytes, 4);
    }

    // =========================================================================
    // Additional coverage tests (bd-3dn2)
    // =========================================================================

    #[test]
    fn event_store_config_default_values() {
        let config = EventStoreConfig::default();
        assert_eq!(config.max_events_per_stream, DEFAULT_MAX_EVENTS_PER_STREAM);
        assert_eq!(config.max_streams, DEFAULT_MAX_STREAMS);
        assert_eq!(config.max_stream_id_bytes, DEFAULT_MAX_STREAM_ID_BYTES);
        assert_eq!(
            config.max_event_payload_bytes,
            DEFAULT_MAX_EVENT_PAYLOAD_BYTES
        );
        assert_eq!(
            config.max_total_event_payload_bytes,
            DEFAULT_MAX_TOTAL_EVENT_PAYLOAD_BYTES
        );
        assert_eq!(config.max_replay_events, DEFAULT_MAX_REPLAY_EVENTS);
        assert_eq!(
            config.max_replay_payload_bytes,
            DEFAULT_MAX_REPLAY_PAYLOAD_BYTES
        );
        assert_eq!(
            config.max_replay_cursor_bytes,
            DEFAULT_MAX_REPLAY_CURSOR_BYTES
        );
        assert_eq!(config.ttl, Some(Duration::from_secs(DEFAULT_TTL_SECS)));
    }

    #[test]
    fn event_store_default_trait() {
        let store = EventStore::default();
        assert_eq!(store.stream_count(), 0);
        assert_eq!(store.event_count(), 0);
        assert_eq!(
            store.config().max_events_per_stream,
            DEFAULT_MAX_EVENTS_PER_STREAM
        );
    }

    #[test]
    fn event_store_config_accessor() {
        let config = EventStoreConfig::no_expiry().max_events(42);
        let store = EventStore::with_config(config).unwrap();
        assert_eq!(store.config().max_events_per_stream, 42);
        assert!(store.config().ttl.is_none());
    }

    #[test]
    fn get_events_after_nonexistent_stream_returns_empty() {
        let store = EventStore::new();
        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        let events = store.get_events_after("no-such-stream", None);
        assert!(events.is_empty());
    }

    #[test]
    fn get_events_after_unknown_id_returns_empty() {
        let store = EventStore::new();
        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        // An unknown after_id returns empty per SSE spec (client should reconnect fresh)
        let events = store.get_events_after("stream1", Some("bogus-id"));
        assert!(events.is_empty());
    }

    #[test]
    fn create_shared_event_store_with_config_works() {
        let config = EventStoreConfig::no_expiry().max_events(5);
        let store = create_shared_event_store_with_config(config).unwrap();
        assert_eq!(store.config().max_events_per_stream, 5);
        assert!(store.config().ttl.is_none());
    }

    #[test]
    fn cleanup_expired_removes_empty_streams() {
        let config = EventStoreConfig {
            max_events_per_stream: 100,
            ttl: Some(Duration::from_secs(1)),
            ..EventStoreConfig::default()
        };
        let store = EventStore::with_config(config).unwrap();

        store
            .store_event("stream1", Some(serde_json::json!({})))
            .unwrap();
        store
            .store_event("stream2", Some(serde_json::json!({})))
            .unwrap();
        assert_eq!(store.stream_count(), 2);

        // Age both retained entries explicitly. A short real-time sleep made
        // the first event eligible for opportunistic cleanup while the test
        // thread was descheduled between the two insertions, so the positive
        // setup itself was nondeterministic under a parallel workspace run.
        let expired_at = Instant::now()
            .checked_sub(Duration::from_secs(2))
            .expect("the monotonic clock can represent a two-second-old event");
        {
            let mut streams = store
                .streams
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for stream in streams.values_mut() {
                for event in &mut stream.events {
                    event.entry.created_at = expired_at;
                }
            }
        }
        store.cleanup_expired();

        // Streams whose events all expired should be removed entirely
        assert_eq!(store.stream_count(), 0);
        assert_eq!(store.event_count(), 0);
    }

    #[test]
    fn event_store_stats_includes_config_fields() {
        let config = EventStoreConfig::no_expiry().max_events(77);
        let store = EventStore::with_config(config).unwrap();
        store.store_event("s1", None).unwrap();

        let stats = store.stats();
        assert_eq!(stats.stream_count, 1);
        assert_eq!(stats.total_events, 1);
        assert_eq!(stats.total_event_payload_bytes, 0);
        assert_eq!(stats.max_events_per_stream, 77);
        assert_eq!(stats.max_streams, DEFAULT_MAX_STREAMS);
        assert_eq!(stats.max_stream_id_bytes, DEFAULT_MAX_STREAM_ID_BYTES);
        assert_eq!(
            stats.max_event_payload_bytes,
            DEFAULT_MAX_EVENT_PAYLOAD_BYTES
        );
        assert_eq!(
            stats.max_total_event_payload_bytes,
            DEFAULT_MAX_TOTAL_EVENT_PAYLOAD_BYTES
        );
        assert_eq!(stats.max_replay_events, DEFAULT_MAX_REPLAY_EVENTS);
        assert_eq!(
            stats.max_replay_payload_bytes,
            DEFAULT_MAX_REPLAY_PAYLOAD_BYTES
        );
        assert_eq!(
            stats.max_replay_cursor_bytes,
            DEFAULT_MAX_REPLAY_CURSOR_BYTES
        );
        assert!(stats.ttl.is_none());
    }

    // =========================================================================
    // Additional coverage tests (bd-1w9n)
    // =========================================================================

    #[test]
    fn event_entry_debug_and_clone() {
        let entry = EventEntry::new("ev1".into(), "s1".into(), Some(serde_json::json!(42)));
        let debug = format!("{entry:?}");
        assert!(debug.contains("ev1"));
        assert!(debug.contains("s1"));

        let cloned = entry.clone();
        assert_eq!(cloned.id, "ev1");
        assert_eq!(cloned.stream_id, "s1");
    }

    #[test]
    fn event_entry_is_expired_not_expired() {
        let entry = EventEntry::new("ev1".into(), "s1".into(), None);
        // Just created → not expired with 1h TTL
        assert!(!entry.is_expired(Some(Duration::from_secs(3600))));
        // Never expires with None TTL
        assert!(!entry.is_expired(None));
    }

    #[test]
    fn event_store_config_debug_and_clone() {
        let config = EventStoreConfig::default().max_events(10);
        let debug = format!("{config:?}");
        assert!(debug.contains("10"));

        let cloned = config.clone();
        assert_eq!(cloned.max_events_per_stream, 10);
    }

    #[test]
    fn event_store_stats_debug_and_clone() {
        let store = EventStore::new();
        store.store_event("s1", None).unwrap();
        let stats = store.stats();
        let debug = format!("{stats:?}");
        assert!(debug.contains("EventStoreStats"));

        let cloned = stats.clone();
        assert_eq!(cloned.stream_count, 1);
    }

    #[test]
    fn event_store_debug() {
        let store = EventStore::new();
        let debug = format!("{store:?}");
        assert!(debug.contains("EventStore"));
    }

    #[test]
    fn cleanup_expired_noop_with_no_ttl() {
        let config = EventStoreConfig::no_expiry();
        let store = EventStore::with_config(config).unwrap();
        store.store_event("s1", Some(serde_json::json!(1))).unwrap();
        store.store_event("s2", Some(serde_json::json!(2))).unwrap();
        store.cleanup_expired();
        // Nothing should be removed
        assert_eq!(store.event_count(), 2);
        assert_eq!(store.stream_count(), 2);
    }

    #[test]
    fn cleanup_removes_empty_stream_even_without_ttl() {
        let store = EventStore::with_config(EventStoreConfig::no_expiry()).unwrap();
        {
            let mut streams = store
                .streams
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            streams.insert("empty".to_string(), StreamEvents::new());
        }

        store.cleanup_expired();

        assert_eq!(store.stream_count(), 0);
    }

    #[test]
    fn clear_stream_nonexistent_is_noop() {
        let store = EventStore::new();
        store.store_event("s1", None).unwrap();
        store.clear_stream("no-such-stream");
        assert_eq!(store.stream_count(), 1);
    }

    #[test]
    fn event_id_format_contains_dash() {
        let store = EventStore::new();
        let id = store.store_event("s1", None).unwrap();
        assert!(id.contains('-'), "event ID should contain a dash: {id}");
    }
}
