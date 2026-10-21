//! The daemon's live model of the speakers, kept current in the background.
//!
//! [`Live::start`] runs the reconcile loop on a thread of its own:
//! - a survey on a schedule, backing off while surveys fail;
//! - GENA subscriptions renewed on time;
//! - every NOTIFY folded into the household model and the playback state;
//! - a rebooted player resubscribed at once;
//! - read-only surveys while the callback listener is unavailable;
//! - each player's health.
//!
//! Callers read snapshots ([`Live::households`], [`Live::player`],
//! [`Live::snapshot`]) without waiting on the network. [`Live::refresh_soon`]
//! asks for a survey now, after a command found a player gone, say.
//! [`Live::subscribe`] pushes each change ([`LiveEvent`]) as it happens, for
//! an event stream. [`Live::stop`] (or dropping the handle) withdraws local
//! subscriptions and arranges best-effort network cleanup.

use crate::HouseholdState;
use crate::events::{Service, Subscriber, Want};
use crate::inventory::DISCOVERY_WAIT;
use crate::playback::{Changes, Playback, PlayerPlayback};
use crate::reconcile::{Health, HealthBoard, Reconciler};
use fsonos_proto::gena::Subscription;
use fsonos_proto::mdns::SonosAdvert;
use fsonos_proto::net::{CallbackEvent, EventSink, Lan, MediaFiles};
use fsonos_proto::ssdp::Advert;
use fsonos_proto::topology::{VanishedDevice, host_of_location};
use fsonos_proto::{Discovered, HttpBody, ProtoError, Transport};
use fsonos_types::PlayerId;
use fsonos_types::text::normalize;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

mod bootstrap;
mod callback;
mod delivery;
mod inventory;
mod observation;
mod progress;
mod pump;

#[cfg(test)]
mod eventless_tests;

/// How [`Live`] runs.
#[derive(Clone)]
pub struct LiveConfig {
    /// Player addresses to survey besides SSDP (seeds.toml, `--seeds`).
    pub seeds: Vec<IpAddr>,
    /// Addresses from an earlier run: also usable to open the event listener.
    /// Each is probed until observed in live topology; the reconciler then
    /// retains its UUID-address hint independently of the live room model.
    pub remembered: Vec<IpAddr>,
    /// How often to survey.
    pub interval: Duration,
    /// The longest a failing survey backs off to (no longer than `interval`
    /// by default: a house that stopped answering is looked for at least as
    /// often as one that answers).
    pub max_backoff: Duration,
    /// The port players send events to (0: any free port).
    pub callback_port: u16,
    /// Clips the event listener also serves at `GET /media/<name>`, so the
    /// players fetch announcements from [`Snapshot::callback`].
    pub media: Option<MediaFiles>,
    /// Called with the households after every survey that found them, so
    /// the caller can remember the players for the next start.
    pub surveyed: Option<Surveyed>,
}

/// See [`LiveConfig::surveyed`].
pub type Surveyed = Arc<dyn Fn(&[HouseholdState]) + Send + Sync>;

impl std::fmt::Debug for LiveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveConfig")
            .field("seeds", &self.seeds)
            .field("remembered", &self.remembered)
            .field("interval", &self.interval)
            .field("max_backoff", &self.max_backoff)
            .field("callback_port", &self.callback_port)
            .field("media", &self.media.is_some())
            .field("surveyed", &self.surveyed.is_some())
            .finish()
    }
}

impl LiveConfig {
    /// Survey every 5 minutes, and after failures no less often; events on
    /// any port.
    #[must_use]
    pub fn new(seeds: Vec<IpAddr>) -> Self {
        Self {
            seeds,
            remembered: Vec::new(),
            interval: Duration::from_mins(5),
            max_backoff: Duration::from_mins(5),
            callback_port: 0,
            media: None,
            surveyed: None,
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
    /// Why the last survey or event listener failed. A successful read-only
    /// survey retains a listener failure until callbacks can be served again.
    pub last_error: Option<String>,
    /// Where players send events, once listening (`http://host:port`);
    /// also the base of clip URLs when [`LiveConfig::media`] is set.
    pub callback: Option<String>,
    /// Players the households list as vanished (powered off), with their
    /// room names.
    pub vanished: Vec<VanishedDevice>,
}

impl Snapshot {
    /// The vanished player whose room is called `name` (compared as room
    /// names are: case, spacing and accents aside), if any: a command to it
    /// is to a speaker that is off, not to an unknown room.
    #[must_use]
    pub fn vanished_room(&self, name: &str) -> Option<&VanishedDevice> {
        let wanted = normalize(name);
        self.vanished
            .iter()
            .find(|v| normalize(&v.zone_name) == wanted)
    }
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
    /// This subscriber missed one or more changes. Read [`Live::snapshot`]
    /// again; the retained prefix is not a complete account of current state.
    /// This is an invalidation, never a command to replay past actions.
    Resync,
}

/// How far a subscriber may fall behind. Overflow coalesces into a
/// [`LiveEvent::Resync`] after the queued prefix, even if no new event arrives.
pub const SUBSCRIBER_BACKLOG: usize = 1024;

type Subscribers = Arc<Mutex<Vec<delivery::Pending>>>;

/// A running live model; see the module docs.
pub struct Live {
    snapshot: Arc<Mutex<Snapshot>>,
    subscribers: Subscribers,
    wake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ready: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// How long [`Live::stop`] waits for the live loop to exit and hand off lease
/// cleanup. Network retirement is bounded and best effort; unreachable players
/// expire whatever could not be ended. An unfinished network call is not joined.
pub const STOP_WAIT: Duration = Duration::from_secs(3);

impl Live {
    /// Start the loop over `lan`: the first survey begins at once.
    ///
    /// # Panics
    /// If the OS refuses to start a thread.
    #[must_use]
    pub fn start(lan: Arc<Lan>, config: LiveConfig) -> Self {
        let transport: Arc<dyn Transport + Send + Sync> = Arc::clone(&lan) as _;
        Self::start_with(transport, lan, config)
    }

    /// Start the loop with `transport` for surveys and reads (one confined to
    /// a routes file, say) and `lan` only for GENA: subscriptions to the
    /// players those surveys found, and the event listener.
    ///
    /// # Panics
    /// If the OS refuses to start a thread.
    #[must_use]
    pub fn start_with(
        transport: Arc<dyn Transport + Send + Sync>,
        lan: Arc<Lan>,
        config: LiveConfig,
    ) -> Self {
        let snapshot = Arc::new(Mutex::new(Snapshot::default()));
        let subscribers: Subscribers = Arc::new(Mutex::new(Vec::new()));
        let wake = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let startup = Arc::clone(&ready);
        let engine = Engine {
            transport,
            lan,
            config,
            snapshot: Arc::clone(&snapshot),
            subscribers: Arc::clone(&subscribers),
            wake: Arc::clone(&wake),
            stop: Arc::clone(&stop),
        };
        let thread = std::thread::Builder::new()
            .name("fsonos-live".into())
            .spawn(move || engine.run(&startup))
            .expect("spawn the live-model thread");
        Self {
            snapshot,
            subscribers,
            wake,
            stop,
            ready,
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

    /// Wait up to `timeout` for the first successful inventory and its initial
    /// playback read attempts to finish. A failed service may remain unknown;
    /// households and accepted events are available while other reads wait.
    #[must_use]
    pub fn wait_ready(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if self.ready.load(Ordering::Acquire) {
                return true;
            }
            if self.stop.load(Ordering::Acquire) {
                return false;
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
    /// unsubscribes. A lagging subscriber receives [`LiveEvent::Resync`] and
    /// must reread the snapshot rather than assume every delta was delivered.
    #[must_use]
    pub fn subscribe(&self) -> Receiver<LiveEvent> {
        let (tx, rx) = sync_channel(SUBSCRIBER_BACKLOG);
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(delivery::Pending::new(tx));
        rx
    }

    /// Stop the live loop and arrange best-effort retirement of its leases.
    pub fn stop(mut self) {
        self.halt();
    }

    /// Ask the loop to stop and wait up to [`STOP_WAIT`] for its cleanup
    /// handoff. Remote subscription expiry is the fallback for failed cleanup.
    fn halt(&mut self) {
        self.stop.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        let deadline = Instant::now() + STOP_WAIT;
        while !thread.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if thread.is_finished() {
            let _ = thread.join();
        } else {
            tracing::warn!(
                wait_s = STOP_WAIT.as_secs(),
                "the live model is still busy with the network; leaving it to finish"
            );
        }
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.halt();
    }
}

struct Engine {
    /// Surveys, reads and M-SEARCH.
    transport: Arc<dyn Transport + Send + Sync>,
    /// GENA only.
    lan: Arc<Lan>,
    config: LiveConfig,
    snapshot: Arc<Mutex<Snapshot>>,
    subscribers: Subscribers,
    wake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// Whether to listen again at `local`, this host's address toward the
/// players now, rather than at `current`. Only when the look before saw
/// `local` too (`moved` remembers it): Wi-Fi dropping out for a moment moves
/// the route to another interface and back within one [`ROUTE_CHECK`], and
/// following each move re-subscribed every player twice (seen live: 47
/// re-opens in a week, nearly all in such pairs 5 s apart). A real move (a
/// new DHCP lease) is followed one look later.
fn route_settled(current: IpAddr, local: IpAddr, moved: &mut Option<IpAddr>) -> bool {
    if local == current {
        *moved = None;
        return false;
    }
    if *moved == Some(local) {
        *moved = None;
        return true;
    }
    *moved = Some(local);
    false
}

/// A player that answered discovery, by SSDP or else by mDNS (on a LAN
/// that drops SSDP multicast mDNS may be all that answers).
fn answering(found: &Discovered) -> Result<IpAddr, String> {
    found
        .ssdp
        .iter()
        .flatten()
        .find_map(|a| host_of_location(&a.location))
        .or_else(|| {
            found
                .mdns
                .iter()
                .flatten()
                .find_map(|a| a.addr.map(IpAddr::V4))
        })
        .ok_or_else(|| match &found.ssdp {
            Err(e) => format!("no player to listen toward: {e}"),
            Ok(_) => "no player answered discovery, and no seeds are configured".to_string(),
        })
}

/// The longest the loop waits for a NOTIFY before looking at its schedule.
const TICK: Duration = Duration::from_millis(200);

/// How often the live model checks which address this host reaches the
/// players from (a local route lookup; nothing is sent).
const ROUTE_CHECK: Duration = Duration::from_secs(5);

/// Successful read-only fallback surveys retry the local listener within
/// this interval. Failed discovery retains its ordinary exponential backoff.
const EVENTLESS_RETRY: Duration = Duration::from_secs(30);

/// A wall-clock gap this much longer than the monotonic one means the host
/// slept.
const SLEEP_GAP: Duration = Duration::from_secs(60);

/// Whether the host slept between two ticks: `Instant` stands still while
/// it sleeps (macOS and Linux), the wall clock does not. Subscriptions
/// renewed by `Instant` can lapse at the players meanwhile.
fn slept(before: (Instant, SystemTime), after: (Instant, SystemTime)) -> bool {
    let monotonic = after.0.saturating_duration_since(before.0);
    let wall = after.1.duration_since(before.1).unwrap_or(Duration::ZERO);
    wall > monotonic + SLEEP_GAP
}

/// What the loop owns.
struct Model {
    /// The previous tick, by both clocks.
    ticked: (Instant, SystemTime),
    /// Bootstrap addresses not yet observed during this run.
    remembered: Vec<IpAddr>,
    rec: Reconciler,
    playback: Playback,
    progress: progress::Watchdog,
    sink: Option<EventSink>,
    surveyed_at: Option<Instant>,
    last_error: Option<String>,
    /// When [`Engine::follow_route`] last looked.
    route_checked: Instant,
    /// A new address toward the players, seen at the last look and not yet
    /// followed ([`route_settled`]).
    route_moved: Option<IpAddr>,
}

/// Surveys and reads through one transport, GENA through the LAN.
struct Via<'a> {
    transport: &'a (dyn Transport + Send + Sync),
    lan: &'a Lan,
}

impl Transport for Via<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.transport
            .soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.transport.http_get(url)
    }

    fn http_get_bytes(&self, url: &str) -> Result<HttpBody, ProtoError> {
        self.transport.http_get_bytes(url)
    }

    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        self.transport.ssdp_search(mx_secs, wait)
    }

    fn mdns_search(&self, wait: Duration) -> Result<Vec<SonosAdvert>, ProtoError> {
        self.transport.mdns_search(wait)
    }

    fn discover(&self, mx_secs: u8, wait: Duration) -> Discovered {
        self.transport.discover(mx_secs, wait)
    }
}

impl Subscriber for Via<'_> {
    fn subscribe(&self, want: &Want, callback_url: &str) -> Result<Subscription, ProtoError> {
        Subscriber::subscribe(self.lan, want, callback_url)
    }

    fn renew(&self, want: &Want, sid: &str) -> Result<Subscription, ProtoError> {
        Subscriber::renew(self.lan, want, sid)
    }

    fn unsubscribe(&self, want: &Want, sid: &str) -> Result<(), ProtoError> {
        Subscriber::unsubscribe(self.lan, want, sid)
    }
}

impl Engine {
    fn via(&self) -> Via<'_> {
        Via {
            transport: &*self.transport,
            lan: &self.lan,
        }
    }

    fn run(self, ready: &AtomicBool) {
        let mut m = Model {
            ticked: (Instant::now(), SystemTime::now()),
            remembered: self.config.remembered.clone(),
            rec: Reconciler::new(
                self.config.interval,
                self.config.max_backoff,
                Instant::now(),
            ),
            playback: Playback::default(),
            progress: progress::Watchdog::default(),
            sink: None,
            surveyed_at: None,
            last_error: None,
            route_checked: Instant::now(),
            route_moved: None,
        };
        m.rec.subscriptions.defer_network();
        let mut surveys = inventory::Surveys::default();
        while !self.stop.load(Ordering::Acquire) {
            // Flush lost-event markers even when the model is quiet. The
            // snapshot was committed before the dropped publication.
            self.emit(&[]);
            let now = Instant::now();
            let wall = SystemTime::now();
            if slept(m.ticked, (now, wall)) {
                tracing::warn!(
                    "this host was asleep; its subscriptions may have lapsed: subscribing afresh"
                );
                self.release(&mut m);
                m.rec.schedule.due_now(now);
            }
            m.ticked = (now, wall);
            if self.wake.swap(false, Ordering::AcqRel) {
                surveys.refresh_soon();
                m.rec.schedule.due_now(now);
            }
            if now.duration_since(m.route_checked) >= ROUTE_CHECK {
                m.route_checked = now;
                self.follow_route(&mut m);
            }
            if m.rec.schedule.due(now) && !surveys.busy() {
                surveys.start(&self, &mut m);
                self.publish(&m, Vec::new());
            }
            if m.sink.is_none() {
                // Neither discovery nor playback reads need a callback sink.
                surveys.complete(&self, &mut m);
                if m.sink.is_some() {
                    pump::turn(&self, &mut m);
                } else {
                    self.poll_renewals(&mut m);
                    progress::recover(&self, &mut m);
                }
            } else {
                pump::turn(&self, &mut m);
                surveys.complete(&self, &mut m);
            }
            // The snapshot is already published, including the last accepted
            // sweep results. Readiness must not race ahead of those read attempts.
            if !ready.load(Ordering::Acquire)
                && !self.stop.load(Ordering::Acquire)
                && progress::finish_bootstrap(&mut m)
            {
                ready.store(true, Ordering::Release);
            }
            if m.sink.is_none() {
                std::thread::sleep(TICK);
            }
        }
        drop(surveys);
        self.release(&mut m);
        self.publish(&m, Vec::new());
    }

    /// Withdraw every lease immediately, forgetting offline players without
    /// calling them. The running controller retires the rest off-thread; a
    /// sleep or route change keeps the same bounded maintenance slots.
    fn release(&self, m: &mut Model) {
        m.rec.invalidate_inventory();
        let offline: Vec<PlayerId> = m
            .rec
            .health
            .iter()
            .filter(|(_, h)| h.health == Health::Offline)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &offline {
            m.rec.subscriptions.forget(id);
        }
        let provider: Arc<dyn Subscriber + Send + Sync> = self.lan.clone();
        m.rec
            .subscriptions
            .release_background(provider, self.stop.load(Ordering::Acquire));
    }

    /// Drop the event listener and every subscription that points at it, so
    /// the next survey listens afresh and subscribes again.
    fn relisten(&self, m: &mut Model, why: &str) {
        tracing::warn!(reason = why, "re-opening the event listener");
        self.release(m);
        m.sink = None;
        m.last_error = Some(why.to_string());
        m.rec.schedule.due_now(Instant::now());
    }

    /// This host's address toward the players can change (a new DHCP lease,
    /// a VPN, Wi-Fi dropping out and the route moving to another interface
    /// and back). Renewals would keep succeeding while every NOTIFY went to
    /// the old address, so listen again where they can reach us, and survey
    /// at once ([`Self::relisten`]): checked every [`ROUTE_CHECK`], not only
    /// at surveys, which back off while the players do not answer. Followed
    /// once two looks agree ([`route_settled`]).
    fn follow_route(&self, m: &mut Model) {
        let Some(s) = &m.sink else {
            m.route_moved = None;
            return;
        };
        let current = s.local_addr().ip();
        let Some(toward) = m
            .rec
            .households
            .iter()
            .flat_map(|h| &h.players)
            .map(|p| p.ip)
            .next()
        else {
            return;
        };
        let Ok(local) = self.lan.local_address_toward(toward) else {
            return;
        };
        if route_settled(current, local, &mut m.route_moved) {
            let why = format!("this host now reaches the players from {local}, not {current}");
            self.relisten(m, &why);
        } else if m.route_moved.is_some() {
            tracing::debug!(%local, %current, "the route toward the players moved; looking again before following it");
        }
    }

    /// Synchronous fixture setup. The live loop uses the inventory and
    /// playback workers from its first survey onward.
    #[cfg(test)]
    fn survey(&self, m: &mut Model, now: Instant) -> Vec<LiveEvent> {
        if self.stop.load(Ordering::Acquire) {
            return Vec::new();
        }
        self.follow_route(m);
        let listener_error = if m.sink.is_none() {
            match self.open_sink(&m.rec.households) {
                Ok(s) => {
                    tracing::info!(callback = %s.callback_url(""), "listening for player events");
                    m.sink = Some(s);
                    None
                }
                Err(error) => {
                    tracing::warn!(%error, "cannot listen for events; surveying without callbacks");
                    Some(error)
                }
            }
        } else {
            None
        };
        if self.stop.load(Ordering::Acquire) {
            return Vec::new();
        }
        // Known households are probed selectively by the reconciler. Only
        // explicit seeds and as-yet-unresolved startup hints are unconditional.
        bootstrap::observed(&mut m.remembered, &m.rec.households);
        let seeds = bootstrap::seeds(&self.config.seeds, &m.remembered);
        let surveyed = match &m.sink {
            Some(s) => {
                let callback = |service: Service| s.callback_url(service.tag());
                m.rec.refresh(&self.via(), &seeds, callback, now)
            }
            None => m.rec.refresh_inventory(&self.via(), &seeds, now),
        };
        match surveyed {
            Ok(report) => {
                bootstrap::observed(&mut m.remembered, &m.rec.households);
                for gone in &report.missing {
                    m.playback.remove(gone);
                }
                if let Some(surveyed) = &self.config.surveyed {
                    surveyed(&m.rec.households);
                }
                let events = observation::survey(self, m);
                m.surveyed_at = Some(now);
                m.last_error = listener_error;
                if m.sink.is_none() {
                    // Count the cooldown from completion: a slow read must
                    // not make the fallback loop immediately overdue again.
                    let completed = Instant::now();
                    m.rec.schedule.succeeded(completed);
                    m.rec.schedule.retry_within(completed, EVENTLESS_RETRY);
                }
                events
            }
            Err(error) => {
                m.last_error = Some(match listener_error {
                    Some(listener) => format!("{listener}; survey failed: {error}"),
                    None => error.to_string(),
                });
                Vec::new()
            }
        }
    }

    /// Admit bounded lease maintenance without waiting for speaker I/O.
    fn renew(&self, m: &mut Model, now: Instant) {
        let Some(s) = &m.sink else { return };
        let callback = |service: Service| s.callback_url(service.tag());
        let provider: Arc<dyn Subscriber + Send + Sync> = self.lan.clone();
        m.rec
            .subscriptions
            .start_renewals(&provider, callback, &self.stop, now);
    }

    fn poll_renewals(&self, m: &mut Model) {
        let reports = m
            .rec
            .subscriptions
            .poll_renewals(self.stop.load(Ordering::Acquire));
        self.lease_reports(m, reports);
    }

    fn lease_reports(&self, m: &mut Model, reports: Vec<(crate::events::Report, Instant)>) {
        if reports.is_empty() {
            return;
        }
        for (mut report, started) in reports {
            // A late failure must not overwrite an event heard while the
            // network call was pending. Subscription cursors stay model-owned.
            let current = |player: &PlayerId| {
                m.rec
                    .health
                    .of(player)
                    .and_then(|h| h.last_ok)
                    .is_none_or(|at| at <= started)
            };
            report.answered.retain(current);
            report.failed.retain(|(player, _, _)| current(player));
            report.unanswered.retain(|(player, _)| current(player));
            m.rec.health.record(&report, Instant::now());
        }
        progress::invalidate(m);
        self.publish(m, Vec::new());
    }

    /// Wait (briefly) for one NOTIFY and fold it in. `None` when none came;
    /// otherwise the playback change it made, if any.
    fn next_event(&self, m: &mut Model, max_wait: Duration) -> Option<Vec<LiveEvent>> {
        m.rec.subscriptions.defer_event_refresh();
        let n = if let Some(early) = m.rec.subscriptions.take_early() {
            // Once a SID is adopted, its retained prefix precedes newer raw
            // callbacks from that same stream.
            early
        } else {
            let buffered = match m.sink.as_ref()?.recv_event(Duration::ZERO) {
                Ok(Some(CallbackEvent::Notify(notify))) => Some(notify),
                Ok(Some(CallbackEvent::ResyncRequired)) => {
                    return Some(self.callback_loss(m));
                }
                Ok(None) => None,
                Err(error) => {
                    self.relisten(m, &format!("the event listener stopped: {error}"));
                    return Some(Vec::new());
                }
            };
            if let Some(notify) = buffered {
                // Consume already-buffered old-SID evidence before deciding
                // whether a replacement subscription can be adopted.
                notify
            } else {
                self.poll_renewals(m);
                if let Some(early) = m.rec.subscriptions.take_early() {
                    early
                } else {
                    // The outer loop checks its schedule every bounded TICK;
                    // a due in-flight survey must not turn this into a spin.
                    match m.sink.as_ref()?.recv_event(max_wait) {
                        Ok(Some(CallbackEvent::Notify(notify))) => notify,
                        Ok(Some(CallbackEvent::ResyncRequired)) => {
                            return Some(self.callback_loss(m));
                        }
                        Ok(None) => return None,
                        Err(error) => {
                            self.relisten(m, &format!("the event listener stopped: {error}"));
                            return Some(Vec::new());
                        }
                    }
                }
            }
        };
        let s = m.sink.as_ref()?;
        if m.rec.subscriptions.hold_early(&n) {
            return Some(Vec::new());
        }
        let callback = |service: Service| s.callback_url(service.tag());
        let from = m
            .rec
            .subscriptions
            .route(&n)
            .map(|(player, service)| (player.clone(), service));
        let mut pushed = Vec::new();
        match m
            .rec
            .on_notify(&self.via(), &mut m.playback, &n, callback, Instant::now())
        {
            Ok(Some(report)) => {
                progress::invalidate_rebooted(m, &report.rebooted);
                bootstrap::observed(&mut m.remembered, &m.rec.households);
                for gone in &report.gone {
                    m.playback.remove(gone);
                }
                if let Some((player, service)) = from {
                    if service == Service::ZoneGroupTopology {
                        progress::refresh_bootstrap(m);
                    }
                    if !report.changes.is_empty() {
                        pushed.push(LiveEvent::Playback {
                            player,
                            changes: report.changes,
                        });
                    }
                }
            }
            Ok(None) => {}
            Err(e) => {
                tracing::debug!(error = %e, sid = n.sid.as_str(), "NOTIFY not applied");
            }
        }
        Some(pushed)
    }

    /// Listen toward a live player, an explicit seed, or a cached address.
    /// Cached-only startup must not depend on multicast before it can survey.
    fn open_sink(&self, households: &[HouseholdState]) -> Result<EventSink, String> {
        let mut candidates = bootstrap::listener_candidates(households, &self.config);
        if candidates.is_empty() {
            candidates.push(answering(&self.transport.discover(1, DISCOVERY_WAIT))?);
        }
        let mut error = "no usable local route to the players".to_string();
        for toward in candidates {
            match self.listen_toward(toward) {
                Ok(sink) => return Ok(sink),
                Err(why) => error = why,
            }
        }
        Err(error)
    }

    fn listen_toward(&self, toward: IpAddr) -> Result<EventSink, String> {
        let local = self
            .lan
            .local_address_toward(toward)
            .map_err(|e| format!("no local route to {toward}: {e}"))?;
        EventSink::start_serving(
            SocketAddr::new(local, self.config.callback_port),
            self.config.media.clone(),
        )
        .map_err(|e| {
            let (port, e) = (self.config.callback_port, e.to_string());
            if e.contains("in use") {
                format!(
                    "cannot listen for events on {local}:{port}: {e}; another program holds \
                     that port (a second fsonos serve on this host?): stop it, or start this \
                     one with another --events-port"
                )
            } else {
                format!("cannot listen for events on {local}: {e}")
            }
        })
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
            vanished: m.rec.vanished.clone(),
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

    /// Push events without blocking; empty batches flush pending resyncs.
    fn emit(&self, events: &[LiveEvent]) {
        self.subscribers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .retain_mut(|subscriber| subscriber.deliver(events));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A player seen only by mDNS, and the bytes of its album art.
    struct MdnsOnly;

    impl Transport for MdnsOnly {
        fn soap_post(
            &self,
            _host: IpAddr,
            _control_path: &str,
            _soap_action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            Err(ProtoError::NotWired("soap_post"))
        }

        fn http_get_bytes(&self, _url: &str) -> Result<HttpBody, ProtoError> {
            Ok(HttpBody {
                content_type: Some("image/png".into()),
                body: vec![7],
            })
        }

        fn mdns_search(&self, _wait: Duration) -> Result<Vec<SonosAdvert>, ProtoError> {
            Ok(vec![SonosAdvert {
                instance: "RINCON_000000000000001400@Living Room".into(),
                uuid: None,
                household: Some("Sonos_example".into()),
                boot_seq: None,
                location: None,
                port: Some(1443),
                addr: Some(std::net::Ipv4Addr::new(192, 0, 2, 10)),
            }])
        }
    }

    /// Listens on both discovery channels at once, and says so by what it
    /// finds.
    struct AtOnce;

    impl Transport for AtOnce {
        fn soap_post(
            &self,
            _host: IpAddr,
            _control_path: &str,
            _soap_action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            Err(ProtoError::NotWired("soap_post"))
        }

        fn discover(&self, _mx_secs: u8, _wait: Duration) -> Discovered {
            Discovered {
                ssdp: Err(ProtoError::NotWired("at once")),
                mdns: Ok(Vec::new()),
            }
        }
    }

    #[test]
    fn events_are_heard_toward_a_player_mdns_found_when_ssdp_found_none() {
        let advert = |addr| SonosAdvert {
            instance: "RINCON_000000000000001400@Living Room".into(),
            uuid: None,
            household: None,
            boot_seq: None,
            location: None,
            port: Some(1443),
            addr,
        };
        let only_mdns = Discovered {
            ssdp: Err(ProtoError::NotWired("ssdp_search")),
            mdns: Ok(vec![advert(Some(std::net::Ipv4Addr::new(192, 0, 2, 10)))]),
        };
        assert_eq!(answering(&only_mdns), Ok("192.0.2.10".parse().unwrap()));
        let both = Discovered {
            ssdp: Ok(vec![Advert {
                location: "http://192.0.2.20:1400/xml/device_description.xml".into(),
                st: fsonos_proto::ssdp::SONOS_ST.into(),
                usn: None,
                household: None,
                boot_seq: None,
            }]),
            mdns: Ok(vec![advert(Some(std::net::Ipv4Addr::new(192, 0, 2, 10)))]),
        };
        assert_eq!(answering(&both), Ok("192.0.2.20".parse().unwrap()));
        let none = Discovered {
            ssdp: Ok(Vec::new()),
            mdns: Ok(vec![advert(None)]),
        };
        assert!(answering(&none).unwrap_err().contains("no seeds"));
    }

    #[test]
    fn the_live_model_discovers_as_its_transport_does() {
        let lan = Lan::start().expect("lan");
        let via = Via {
            transport: &AtOnce,
            lan: &lan,
        };
        let found = via.discover(1, Duration::ZERO);
        assert!(matches!(found.ssdp, Err(ProtoError::NotWired("at once"))));
        assert!(found.mdns.is_ok_and(|m| m.is_empty()));
    }

    #[test]
    fn the_live_model_surveys_and_reads_through_every_channel_of_its_transport() {
        let lan = Lan::start().expect("lan");
        let via = Via {
            transport: &MdnsOnly,
            lan: &lan,
        };
        // The survey's second discovery channel (see `inventory`).
        let seen = via
            .mdns_search(Duration::ZERO)
            .expect("mdns through the live model");
        assert_eq!(seen[0].household.as_deref(), Some("Sonos_example"));
        assert_eq!(
            via.http_get_bytes("http://192.0.2.10:1400/art")
                .unwrap()
                .body,
            [7]
        );
    }

    #[test]
    fn a_wall_clock_jump_past_the_monotonic_clock_is_a_sleep() {
        let (t0, w0) = (Instant::now(), SystemTime::now());
        let step = Duration::from_secs(1);
        assert!(!slept((t0, w0), (t0 + step, w0 + step)), "both advanced");
        assert!(
            !slept((t0, w0), (t0 + step, w0 + Duration::from_secs(30))),
            "under a minute of drift"
        );
        assert!(
            slept((t0, w0), (t0 + step, w0 + Duration::from_mins(20))),
            "twenty minutes asleep"
        );
        assert!(
            !slept((t0, w0 + Duration::from_secs(600)), (t0 + step, w0)),
            "the wall clock set back is no sleep"
        );
    }

    /// The live pattern: the route leaves for another interface and comes
    /// back one look later. Nothing to follow; a move that stays is
    /// followed once, a look later.
    #[test]
    fn a_route_that_comes_straight_back_is_not_followed() {
        let wifi: IpAddr = "192.0.2.10".parse().unwrap();
        let wired: IpAddr = "198.51.100.20".parse().unwrap();
        let renewed: IpAddr = "192.0.2.11".parse().unwrap();
        let mut moved = None;
        let mut follow = |local| route_settled(wifi, local, &mut moved);
        assert!(!follow(wifi));
        // A blip: away for one look, then back.
        assert!(!follow(wired));
        assert!(!follow(wifi));
        assert!(!follow(wired), "the earlier blip does not count");
        // Away, then somewhere else: still not settled.
        assert!(!follow(renewed));
        // The same new address twice in a row: follow it, once.
        assert!(follow(renewed));
        assert!(!follow(wifi));
        assert!(!follow(renewed));
        assert!(follow(renewed));
    }

    #[test]
    fn a_house_that_stopped_answering_is_looked_for_as_often_as_one_that_answers() {
        let config = LiveConfig::new(Vec::new());
        assert!(config.max_backoff <= config.interval, "{config:?}");
        assert!(ROUTE_CHECK < config.interval);
    }
}
