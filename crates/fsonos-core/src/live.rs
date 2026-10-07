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
//! [`Live::stop`] (or dropping the handle) ends every subscription.

use crate::HouseholdState;
use crate::events::Service;
use crate::inventory::DISCOVERY_WAIT;
use crate::playback::{Playback, PlayerPlayback};
use crate::reconcile::{HealthBoard, Reconciler};
use fsonos_proto::Transport;
use fsonos_proto::net::{EventSink, Lan};
use fsonos_proto::topology::host_of_location;
use fsonos_types::PlayerId;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, Ordering};
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

/// A running live model; see the module docs.
pub struct Live {
    snapshot: Arc<Mutex<Snapshot>>,
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
        let wake = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let engine = Engine {
            lan,
            config,
            snapshot: Arc::clone(&snapshot),
            wake: Arc::clone(&wake),
            stop: Arc::clone(&stop),
        };
        let thread = std::thread::Builder::new()
            .name("fsonos-live".into())
            .spawn(move || engine.run())
            .expect("spawn the live-model thread");
        Self {
            snapshot,
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
    wake: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}

/// The longest the loop waits for a NOTIFY before looking at its schedule.
const TICK: Duration = Duration::from_millis(200);

impl Engine {
    fn run(self) {
        let lan = &*self.lan;
        let mut rec = Reconciler::new(
            self.config.interval,
            self.config.max_backoff,
            Instant::now(),
        );
        let mut playback = Playback::default();
        let mut sink: Option<EventSink> = None;
        let mut surveyed_at = None;
        let mut last_error = None;
        while !self.stop.load(Ordering::Acquire) {
            let now = Instant::now();
            if self.wake.swap(false, Ordering::AcqRel) {
                rec.schedule.due_now(now);
            }
            if rec.schedule.due(now) {
                if sink.is_none() {
                    match self.open_sink(&rec.households) {
                        Ok(s) => {
                            tracing::info!(callback = %s.callback_url(""), "listening for player events");
                            sink = Some(s);
                        }
                        Err(e) => {
                            let retry = rec.schedule.failed(now);
                            tracing::warn!(error = %e, retry_in_s = retry.as_secs(), "cannot listen for events yet");
                            last_error = Some(e);
                        }
                    }
                }
                if let Some(s) = &sink {
                    let callback = |service: Service| s.callback_url(service.tag());
                    match rec.refresh(lan, &self.config.seeds, callback, now) {
                        Ok(report) => {
                            for gone in &report.missing {
                                playback.remove(gone);
                            }
                            surveyed_at = Some(now);
                            last_error = None;
                        }
                        Err(e) => last_error = Some(e.to_string()),
                    }
                }
                self.publish(
                    &rec,
                    &playback,
                    sink.as_ref(),
                    surveyed_at,
                    last_error.as_ref(),
                );
            }
            let Some(s) = &sink else {
                std::thread::sleep(TICK);
                continue;
            };
            let callback = |service: Service| s.callback_url(service.tag());
            if rec.subscriptions.next_due().is_some_and(|due| due <= now) {
                let report = rec.subscriptions.renew_due(lan, callback, now);
                rec.health.record(&report);
                self.publish(
                    &rec,
                    &playback,
                    sink.as_ref(),
                    surveyed_at,
                    last_error.as_ref(),
                );
            }
            let wait = rec
                .schedule
                .next_at()
                .saturating_duration_since(Instant::now())
                .min(TICK);
            if let Some(n) = s.recv_timeout(wait) {
                match rec.on_notify(lan, &mut playback, &n, callback, Instant::now()) {
                    Ok(Some(report)) => {
                        for gone in &report.gone {
                            playback.remove(gone);
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::debug!(error = %e, sid = n.sid.as_str(), "NOTIFY not applied");
                    }
                }
                self.publish(
                    &rec,
                    &playback,
                    sink.as_ref(),
                    surveyed_at,
                    last_error.as_ref(),
                );
            }
        }
        rec.subscriptions.unsubscribe_all(lan);
        self.publish(
            &rec,
            &playback,
            sink.as_ref(),
            surveyed_at,
            last_error.as_ref(),
        );
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

    fn publish(
        &self,
        rec: &Reconciler,
        playback: &Playback,
        sink: Option<&EventSink>,
        surveyed_at: Option<Instant>,
        last_error: Option<&String>,
    ) {
        let next = Snapshot {
            households: rec.households.clone(),
            playback: playback.clone(),
            health: rec.health.clone(),
            subscriptions: rec.subscriptions.len(),
            surveyed_at,
            last_error: last_error.cloned(),
            callback: sink.map(|s| s.callback_url("").trim_end_matches('/').to_string()),
        };
        *self.snapshot.lock().unwrap_or_else(PoisonError::into_inner) = next;
    }
}
