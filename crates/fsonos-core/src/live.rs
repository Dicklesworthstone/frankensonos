//! The daemon's live model of the speakers, kept current in the background.
//!
//! [`Live::start`] runs the reconcile loop on a thread of its own:
//! - a survey on a schedule, backing off while surveys fail;
//! - GENA subscriptions renewed on time;
//! - every NOTIFY folded into the household model and the playback state;
//! - a rebooted player resubscribed at once;
//! - each player's health.
//!
//! Callers read snapshots ([`Live::households`], [`Live::player`],
//! [`Live::snapshot`]) without waiting on the network. [`Live::refresh_soon`]
//! asks for a survey now, after a command found a player gone, say.
//! [`Live::subscribe`] pushes each change ([`LiveEvent`]) as it happens, for
//! an event stream. [`Live::stop`] (or dropping the handle) ends every
//! subscription.

use crate::HouseholdState;
use crate::events::Service;
use crate::inventory::DISCOVERY_WAIT;
use crate::playback::{Changes, Playback, PlayerPlayback};
use crate::reconcile::{Health, HealthBoard, Reconciler};
use fsonos_proto::Transport;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::host_of_location;
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How [`Live`] runs.
#[derive(Debug, Clone)]
pub struct LiveConfig {
    /// Player addresses to survey besides SSDP (seeds.toml, `--seeds`).
    pub seeds: Vec<IpAddr>,
    /// How often to survey.
    pub interval: Duration,
    /// The longest a failing survey backs off to.
    pub max_backoff: Duration,
    /// The port players send events to (0: any free port).
    pub callback_port: u16,
}

impl LiveConfig {
    /// Survey every 5 minutes, back off to at most 30, events on any port.
    #[must_use]
    pub fn new(seeds: Vec<IpAddr>) -> Self {
        Self {
            seeds,
            interval: Duration::from_mins(5),
            max_backoff: Duration::from_mins(30),
            callback_port: 0,
        }
    }
}

/// The live model at one moment.
#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub households: Vec<HouseholdState>,
    pub playback: Playback,
    pub health: HealthBoard,
    /// Active GENA subscriptions.
    pub subscriptions: usize,
    /// When a survey last succeeded.
    pub surveyed_at: Option<Instant>,
    /// Why the last survey (or the event listener) failed, until one works.
    pub last_error: Option<String>,
    /// Where players send events, once listening (`http://host:port`).
    pub callback: Option<String>,
}

/// A change the live model saw, pushed to every [`Live::subscribe`]r.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveEvent {
    /// A player's playback changed: transport, track, volume, mute or group
    /// volume.
    Playback { player: PlayerId, changes: Changes },
    /// The households changed: grouping, rooms, or players joining, leaving
    /// or moving. Read [`Live::households`] for the new shape.
    Topology,
    /// A player's health changed (it went offline or came back, say).
    Health { player: PlayerId, health: Health },
}

/// How far a subscriber may fall behind; newer events are dropped for it
/// until it catches up (it can resync from [`Live::snapshot`]).
pub const SUBSCRIBER_BACKLOG: usize = 1024;

type Subscribers = Arc<Mutex<Vec<SyncSender<LiveEvent>>>>;

/// A running live model; see the module docs.
pub struct Live {
    snapshot: Arc<Mutex<Snapshot>>,
    subscribers: Subscribers,
    wake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Live {
    /// Start the loop: the first survey begins at once.
    ///
    /// # Panics
    /// If the OS refuses to start a thread.
    #[must_use]
    pub fn start(lan: Arc<Lan>, config: LiveConfig) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let wake = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Engine {
            lan,
            config,
            snapshot: Arc::clone(&snapshot),
            subscribers: Arc::clone(&subscribers),
            wake: Arc::clone(&wake),
            stop: Arc::clone(&stop),
        };
        let thread = std::thread::Builder::new()
            .name("fsonos-live".into())
            .spawn(move || engine.run())
            .expect("spawn the live-model thread");
        Self {
            snapshot,
            subscribers,
            wake,
            stop,
            thread: Some(thread),
        }
    }

    /// The whole model as of now.
    #[must_use]
    pub fn snapshot(&self) -> Snapshot {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The households as last surveyed and updated by topology events.
    #[must_use]
    pub fn households(&self) -> Vec<HouseholdState> {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .households
            .clone()
    }

    /// `player`'s playback state, once it has reported any.
    #[must_use]
    pub fn player(&self, player: &PlayerId) -> Option<PlayerPlayback> {
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .playback
            .of(player)
            .cloned()
    }

    /// Wait up to `timeout` for the first successful survey.
    #[must_use]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if !self.households().is_empty() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Survey as soon as the loop is free (a command found a player gone).
    pub fn refresh_soon(&self) {
        self.wake.store(true, Ordering::Release);
    }

    /// Every change from now on, as it happens. Dropping the receiver
    /// unsubscribes; one that falls [`SUBSCRIBER_BACKLOG`] behind misses
    /// events until it catches up.
    #[must_use]
    pub fn subscribe(&self) -> Receiver<LiveEvent> {
        let (tx, rx) = sync_channel(SUBSCRIBER_BACKLOG);
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tx);
        rx
    }

    /// End every subscription and stop the loop.
    pub fn stop(mut self) {
        self.halt();
    }

    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.halt();
    }
}

struct Engine {
    lan: Arc<Lan>,
    config: LiveConfig,
    snapshot: Arc<Mutex<Snapshot>>,
    subscribers: Subscribers,
    wake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// The longest the loop waits for a NOTIFY before looking at its schedule.
const TICK: Duration = Duration::from_millis(200);

/// What the loop owns.
struct Model {
    rec: Reconciler,
    playback: Playback,
    sink: Option<EventSink>,
    surveyed_at: Option<Instant>,
    last_error: Option<String>,
}

impl Engine {
    fn run(self) {
        let mut m = Model {
            rec: Reconciler::new(
                self.config.interval,
                self.config.max_backoff,
                Instant::now(),
            ),
            playback: Playback::default(),
            sink: None,
            surveyed_at: None,
            last_error: None,
        };
        while !self.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if self.wake.swap(false, Ordering::AcqRel) {
                m.rec.schedule.due_now(now);
            }
            if m.rec.schedule.due(now) {
                self.survey(&mut m, now);
                self.publish(&m, Vec::new());
            }
            if m.sink.is_none() {
                std::thread::sleep(TICK);
                continue;
            }
            if m.rec.subscriptions.next_due().is_some_and(|due| due <= now) {
                self.renew(&mut m, now);
                self.publish(&m, Vec::new());
            }
            if let Some(pushed) = self.next_event(&mut m) {
                self.publish(&m, pushed);
            }
        }
        m.rec.subscriptions.unsubscribe_all(&*self.lan);
        self.publish(&m, Vec::new());
    }

    /// Survey, opening the event listener first if there is none yet.
    fn survey(&self, m: &mut Model, now: Instant) {
        if m.sink.is_none() {
            match self.open_sink(&m.rec.households) {
                Ok(s) => {
                    tracing::info!(callback = %s.callback_url(""), "listening for player events");
                    m.sink = Some(s);
                }
                Err(e) => {
                    let retry = m.rec.schedule.failed(now);
                    tracing::warn!(error = %e, retry_in_s = retry.as_secs(), "cannot listen for events yet");
                    m.last_error = Some(e);
                    return;
                }
            }
        }
        let Some(s) = &m.sink else { return };
        let callback = |service: Service| s.callback_url(service.tag());
        match m.rec.refresh(&*self.lan, &self.config.seeds, callback, now) {
            Ok(report) => {
                for gone in &report.missing {
                    m.playback.remove(gone);
                }
                m.surveyed_at = Some(now);
                m.last_error = None;
            }
            Err(e) => m.last_error = Some(e.to_string()),
        }
    }

    /// Renew the subscriptions that are due.
    fn renew(&self, m: &mut Model, now: Instant) {
        let Some(s) = &m.sink else { return };
        let callback = |service: Service| s.callback_url(service.tag());
        let report = m.rec.subscriptions.renew_due(&*self.lan, callback, now);
        m.rec.health.record(&report);
    }

    /// Wait (briefly) for one NOTIFY and fold it in. `None` when none came;
    /// otherwise the playback change it made, if any.
    fn next_event(&self, m: &mut Model) -> Option<Vec<LiveEvent>> {
        let s = m.sink.as_ref()?;
        let wait = m
            .rec
            .schedule
            .next_at()
            .saturating_duration_since(Instant::now())
            .min(TICK);
        let n = s.recv_timeout(wait)?;
        let callback = |service: Service| s.callback_url(service.tag());
        let from = m.rec.subscriptions.route(&n).map(|(p, _)| p.clone());
        let mut pushed = Vec::new();
        match m
            .rec
            .on_notify(&*self.lan, &mut m.playback, &n, callback, Instant::now())
        {
            Ok(Some(report)) => {
                for gone in &report.gone {
                    m.playback.remove(gone);
                }
                if let Some(player) = from
                    && !report.changes.is_empty()
                {
                    pushed.push(LiveEvent::Playback {
                        player,
                        changes: report.changes,
                    });
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, sid = n.sid.as_str(), "NOTIFY not applied");
            }
        }
        Some(pushed)
    }

    /// Listen for events on the address this host uses to reach the
    /// players: toward a known player, else a seed, else whoever answers
    /// an M-SEARCH.
    fn open_sink(&self, households: &[HouseholdState]) -> Result<EventSink, String> {
        let known = households
            .iter()
            .flat_map(|h| &h.players)
            .map(|p| p.ip)
            .next()
            .or_else(|| self.config.seeds.first().copied());
        let toward = match known {
            Some(ip) => ip,
            None => self
                .lan
                .ssdp_search(1, DISCOVERY_WAIT)
                .map_err(|e| format!("no player to listen toward: {e}"))?
                .iter()
                .find_map(|a| host_of_location(&a.location))
                .ok_or("no player answered M-SEARCH, and no seeds are configured")?,
        };
        let local = self
            .lan
            .local_address_toward(toward)
            .map_err(|e| format!("no local route to {toward}: {e}"))?;
        EventSink::start(SocketAddr::new(local, self.config.callback_port))
            .map_err(|e| format!("cannot listen for events on {local}: {e}"))
    }

    fn publish(&self, m: &Model, mut events: Vec<LiveEvent>) {
        let next = Snapshot {
            households: m.rec.households.clone(),
            playback: m.playback.clone(),
            health: m.rec.health.clone(),
            subscriptions: m.rec.subscriptions.len(),
            surveyed_at: m.surveyed_at,
            last_error: m.last_error.clone(),
            callback: m
                .sink
                .as_ref()
                .map(|s| s.callback_url("").trim_end_matches('/').to_string()),
        };
        let mut current = self.snapshot.lock().unwrap_or_else(PoisonError::into_inner);
        let mut changed = Vec::new();
        if current.households != next.households {
            changed.push(LiveEvent::Topology);
        }
        let before: HashMap<&PlayerId, Health> = current
            .health
            .iter()
            .map(|(id, h)| (id, h.health))
            .collect();
        for (id, h) in next.health.iter() {
            let news = match before.get(id) {
                Some(&was) => was != h.health,
                None => h.health != Health::Healthy,
            };
            if news {
                changed.push(LiveEvent::Health {
                    player: id.clone(),
                    health: h.health,
                });
            }
        }
        changed.append(&mut events);
        *current = next;
        drop(current);
        self.emit(&changed);
    }

    /// Push `events` to every subscriber, forgetting those that hung up.
    fn emit(&self, events: &[LiveEvent]) {
        if events.is_empty() {
            return;
        }
        let mut subscribers = self
            .subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        subscribers.retain(|tx| {
            events
                .iter()
                .all(|e| !matches!(tx.try_send(e.clone()), Err(TrySendError::Disconnected(_))))
        });
    }
}
