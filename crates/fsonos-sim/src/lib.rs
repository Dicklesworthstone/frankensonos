//! Virtual Sonos players over real localhost sockets.
//!
//! A test double for FrankenSonos's own client. Each virtual player is an
//! asupersync HTTP/1.1 server on `127.0.0.1:<ephemeral port>` that serves its
//! device description and answers the community-documented `:1400` SOAP
//! control surface (AVTransport, RenderingControl, GroupRenderingControl,
//! ZoneGroupTopology, ContentDirectory, DeviceProperties) from its own state,
//! with faults shaped like real players'. Everything is synthetic:
//! `RINCON_000E58A0…` ids, made-up rooms and favorites, and per-household
//! Spotify render parameters that the client must learn from `FV:2` before a
//! Spotify item renders. No firmware, binaries, or captures are involved.
//!
//! ```no_run
//! use fsonos_proto::topology::get_zone_group_state;
//!
//! let sim = fsonos_sim::SimHousehold::standard().spawn().unwrap();
//! let kitchen = sim.transport("Kitchen").unwrap();
//! let state = get_zone_group_state(&kitchen, kitchen.ip()).unwrap();
//! assert!(!state.groups.is_empty());
//! ```
//!
//! Players are addressed by base URL, not IP: every virtual player shares
//! `127.0.0.1`, so [`SimTransport`] is per player.
//!
//! Time is a [`SimClock`] that moves only when told. [`SimHandle::advance`]
//! moves it the way players live it: a queue track that ends starts the
//! next, a clip that plays out stops, and a sleep timer that runs out pauses
//! its group, each evented at its own moment. A player told to play a
//! loopback `http://` URI fetches it ([`SimHandle::fetch_log`]); one it
//! cannot fetch leaves it STOPPED, as on a real speaker.
//!
//! Faults, for scenarios that need things to go wrong:
//! [`upnp_fault`](SimHandle::upnp_fault), [`set_latency`](SimHandle::set_latency),
//! [`drop_notifies`](SimHandle::drop_notifies), [`reboot`](SimHandle::reboot),
//! [`change_address`](SimHandle::change_address),
//! [`reelect_coordinator`](SimHandle::reelect_coordinator),
//! [`set_offline`](SimHandle::set_offline), [`join_lag`](SimHandle::join_lag),
//! [`retain_favorites`](SimHandle::retain_favorites).

mod docs;
mod fetch;
mod gena;
mod model;
mod server;
mod ssdp;

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_proto::ssdp::{Advert, m_search, parse_response};
use fsonos_proto::{ProtoError, Transport};
use model::{Favorite, Household, State};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// The hardware a virtual player imitates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimModel {
    /// Play:5 Gen 1 (model `S5`), S1-only.
    Play5Gen1,
    /// Bridge (`ZB100`), S1-only; not a renderer.
    Bridge,
    /// One (`S13`).
    One,
    /// Play:1 (`S1`, a model number, not the generation).
    Play1,
}

impl SimModel {
    #[must_use]
    pub fn model_name(self) -> &'static str {
        match self {
            Self::Play5Gen1 => "Sonos Play:5",
            Self::Bridge => "Sonos Bridge",
            Self::One => "Sonos One",
            Self::Play1 => "Sonos Play:1",
        }
    }

    #[must_use]
    pub fn model_number(self) -> &'static str {
        match self {
            Self::Play5Gen1 => "S5",
            Self::Bridge => "ZB100",
            Self::One => "S13",
            Self::Play1 => "S1",
        }
    }

    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Play5Gen1 => "Play:5",
            Self::Bridge => "Bridge",
            Self::One => "One",
            Self::Play1 => "Play:1",
        }
    }

    /// Whether it plays audio (exposes AVTransport). False for a Bridge.
    #[must_use]
    pub fn is_renderer(self) -> bool {
        self != Self::Bridge
    }
}

/// One room to simulate: a single player, or a stereo pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimPlayerSpec {
    pub room: String,
    pub model: SimModel,
    /// Two players bonded as one room: a visible primary (LF) and a hidden
    /// secondary (RF) that follows it through every group change.
    pub stereo_pair: bool,
}

impl SimPlayerSpec {
    #[must_use]
    pub fn new(room: &str, model: SimModel) -> Self {
        Self {
            room: room.to_string(),
            model,
            stereo_pair: false,
        }
    }

    /// A stereo pair of `model` in `room`.
    #[must_use]
    pub fn pair(room: &str, model: SimModel) -> Self {
        Self {
            stereo_pair: true,
            ..Self::new(room, model)
        }
    }
}

/// The simulator's clock, in milliseconds. It only moves when advanced, so
/// playback positions are deterministic.
#[derive(Debug, Clone, Default)]
pub struct SimClock(Arc<AtomicU64>);

impl SimClock {
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }

    pub fn advance(&self, by: Duration) {
        let ms = u64::try_from(by.as_millis()).unwrap_or(u64::MAX);
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

/// One SOAP request the simulator answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapLogEntry {
    pub player: String,
    pub room: String,
    pub service: String,
    pub action: String,
    pub args: Vec<(String, String)>,
    /// Out-arguments, or the UPnP error code of the fault returned.
    pub result: Result<Vec<(String, String)>, u16>,
    /// [`SimClock`] time of the request.
    pub at_ms: u64,
}

/// A media fetch a player made when told to play a loopback `http://` URI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchLogEntry {
    pub player: String,
    pub room: String,
    pub url: String,
    /// The HTTP status, or why the request failed. A failure (or a status of
    /// 400 or more) stops the player if it is still on that URI.
    pub result: Result<u16, String>,
    pub bytes: usize,
    /// The length, when the body is a PCM WAV; the player then uses it.
    pub wav_duration_ms: Option<u64>,
    /// [`SimClock`] time the fetch finished.
    pub at_ms: u64,
}

/// One GENA event the simulator recorded.
#[derive(Debug, Clone, PartialEq)]
pub struct GenaLogEntry {
    /// [`SimClock`] time.
    pub at_ms: u64,
    pub player: String,
    /// `AVTransport`, `RenderingControl`, `ZoneGroupTopology`, or empty for
    /// player-level events (reboots and the like).
    pub service: String,
    pub event: GenaEvent,
}

/// What happened, in a [`GenaLogEntry`].
#[derive(Debug, Clone, PartialEq)]
pub enum GenaEvent {
    Subscribed {
        sid: String,
        callback: String,
        timeout_secs: u32,
    },
    Renewed {
        sid: String,
        timeout_secs: u32,
    },
    Unsubscribed {
        sid: String,
    },
    /// A SUBSCRIBE / UNSUBSCRIBE the player turned away.
    Refused {
        status: u16,
        reason: String,
    },
    /// The subscription lapsed without renewal.
    Expired {
        sid: String,
    },
    /// A NOTIFY was queued for delivery.
    Notified {
        sid: String,
        seq: u32,
    },
    /// A NOTIFY swallowed by [`SimHandle::drop_notifies`] (its SEQ is spent).
    Dropped {
        sid: String,
        seq: u32,
    },
    /// The subscriber answered a NOTIFY with `status`.
    Delivered {
        sid: String,
        seq: u32,
        status: u16,
    },
    DeliveryFailed {
        sid: String,
        seq: u32,
        error: String,
    },
    Rebooted,
    WentOffline,
    CameOnline,
    AddressChanged {
        from: IpAddr,
        to: IpAddr,
    },
    /// This player handed coordination of its group to `to`.
    CoordinatorReelected {
        to: String,
    },
}

/// Which NOTIFYs a player swallows.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum NotifyDrop {
    /// The next `n`.
    Next(u32),
    /// Each with this probability (a fixed-seed generator, so runs repeat).
    Probability(f64),
}

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("cannot start the simulator: {0}")]
    Start(String),
    #[error("no virtual player {0:?}")]
    UnknownPlayer(String),
    #[error("{0}")]
    Invalid(String),
}

/// A running virtual player.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimPlayerInfo {
    pub uuid: String,
    pub room: String,
    pub model: SimModel,
    /// 1 = S1, 2 = S2.
    pub generation: u8,
    pub household: String,
    /// The address the player advertises (`192.0.2.N`, port 1400); reach it
    /// through [`SimHandle::lan`].
    pub ip: IpAddr,
    /// Whether it is the hidden half of a stereo pair.
    pub hidden: bool,
    /// The real loopback socket it listens on.
    pub addr: SocketAddr,
    /// `http://127.0.0.1:<port>`.
    pub base_url: String,
}

/// Entry point: a builder for one or two virtual households.
#[derive(Debug)]
pub struct SimHousehold;

impl SimHousehold {
    /// No households yet; add them with [`SimBuilder::s1`] / [`SimBuilder::s2`].
    #[must_use]
    pub fn builder() -> SimBuilder {
        SimBuilder::default()
    }

    /// An S1 household (Kitchen and Office Play:5s plus a Bridge) and an S2
    /// household (Living Room One, Bedroom Play:1).
    #[must_use]
    pub fn standard() -> SimBuilder {
        Self::builder()
            .s1([
                SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
                SimPlayerSpec::new("Office", SimModel::Play5Gen1),
                SimPlayerSpec::new("Bridge", SimModel::Bridge),
            ])
            .s2([
                SimPlayerSpec::new("Living Room", SimModel::One),
                SimPlayerSpec::new("Bedroom", SimModel::Play1),
            ])
    }
}

/// Collects households, then [`spawn`](Self::spawn)s them.
#[derive(Debug, Default)]
pub struct SimBuilder {
    households: Vec<(u8, Vec<SimPlayerSpec>)>,
}

impl SimBuilder {
    /// Add an S1 household with `players`.
    #[must_use]
    pub fn s1(mut self, players: impl IntoIterator<Item = SimPlayerSpec>) -> Self {
        self.households.push((1, players.into_iter().collect()));
        self
    }

    /// Add an S2 household with `players`.
    #[must_use]
    pub fn s2(mut self, players: impl IntoIterator<Item = SimPlayerSpec>) -> Self {
        self.households.push((2, players.into_iter().collect()));
        self
    }

    /// Start one listener per player. Each player starts as the coordinator
    /// of its own group, stopped, at volume 20, with an empty queue.
    pub fn spawn(self) -> Result<SimHandle, SimError> {
        let clock = SimClock::default();
        let mut state = State::new(
            self.households
                .iter()
                .map(|(sw_gen, _)| household(*sw_gen))
                .collect(),
            clock.clone(),
        );
        for (h, (_, players)) in self.households.iter().enumerate() {
            for spec in players {
                if spec.stereo_pair {
                    state.add_pair(&spec.room, spec.model, h);
                } else {
                    state.add_player(&spec.room, spec.model, h);
                }
            }
        }
        let (notify_tx, notify_rx) = mpsc::channel();
        state.notifier = Some(notify_tx);
        let state = Arc::new(Mutex::new(state));
        let count = state.lock().map_or(0, |s| s.players.len());
        let notifier = gena::start_notifier(Arc::clone(&state), notify_rx)
            .map_err(|e| SimError::Start(e.to_string()))?;
        let responder = ssdp::Responder::start(Arc::clone(&state))
            .map_err(|e| SimError::Start(e.to_string()))?;
        let mut handle = SimHandle {
            state: Arc::clone(&state),
            players: Vec::with_capacity(count),
            listeners: Vec::with_capacity(count),
            notifier: Some(notifier),
            responder,
            clock,
        };
        for index in 0..count {
            // On error, `handle` drops and stops the listeners already started.
            let listener = server::start(Arc::clone(&state), index)?;
            let addr = listener.addr;
            handle.listeners.push(listener);
            let mut s = state
                .lock()
                .map_err(|_| SimError::Start("state lock poisoned".into()))?;
            s.players[index].port = addr.port();
            let p = &s.players[index];
            handle.players.push(SimPlayerInfo {
                uuid: p.uuid.clone(),
                room: p.room.clone(),
                model: p.model,
                generation: s.households[p.household].sw_gen,
                household: s.households[p.household].id.clone(),
                ip: IpAddr::V4(p.ip),
                hidden: p.pair_primary.is_some(),
                addr,
                base_url: format!("http://{addr}"),
            });
        }
        Ok(handle)
    }
}

/// The synthetic household of generation `sw_gen`: its id, the Spotify
/// render parameters its linked account uses, and its favorites.
fn household(sw_gen: u8) -> Household {
    let (flags, sn, prefix) = if sw_gen == 1 {
        (8224, 1, "10032020")
    } else {
        (8232, 2, "10032028")
    };
    let params = SpotifyRenderParams {
        sid: 12,
        flags,
        sn,
        cdudn: "SA_RINCON3079_X_#Svc3079-0-Token".to_string(),
        item_id_prefix: prefix.to_string(),
    };
    let spotify_id = |tag: &str| format!("{:0<22}", format!("0SimS{sw_gen}{tag}"));
    let query = |flags: u32| format!("?sid={}&flags={flags}&sn={sn}", params.sid);
    let track = |tag: &str, title: &str, by: &str| {
        let enc = format!("spotify%3atrack%3a{}", spotify_id(tag));
        Favorite {
            title: title.to_string(),
            uri: format!("x-sonos-spotify:{enc}{}", query(flags)),
            protocol_info: docs::protocol_info("x-sonos-spotify:"),
            metadata: docs::item_metadata(
                &format!("{prefix}{enc}"),
                title,
                "object.item.audioItem.musicTrack",
                &params.cdudn,
            ),
            kind: "instantPlay",
            description: format!("By {by}"),
        }
    };
    let container = |kind: &str, code: &str, tag: &str, title: &str, class: &str| {
        let enc = format!("{code}spotify%3a{kind}%3a{}", spotify_id(tag));
        Favorite {
            title: title.to_string(),
            uri: format!("x-rincon-cpcontainer:{enc}{}", query(8300)),
            protocol_info: docs::protocol_info("x-rincon-cpcontainer:"),
            metadata: docs::item_metadata(&enc, title, class, &params.cdudn),
            kind: "instantPlay",
            description: "Spotify".to_string(),
        }
    };
    let favorites = vec![
        track("TrackA", "Nocturne in E-flat", "Sim Pianist"),
        track("TrackB", "Aria", "Sim Harpsichordist"),
        container(
            "album",
            "1004206c",
            "Album",
            "Sim Symphonies",
            "object.container.album.musicAlbum",
        ),
        container(
            "playlist",
            "1006206c",
            "List",
            "Sim Quartets",
            "object.container.playlistContainer",
        ),
        Favorite {
            title: "Sim Radio".to_string(),
            uri: "x-rincon-mp3radio://stream.example.invalid/sim.mp3".to_string(),
            protocol_info: docs::protocol_info("x-rincon-mp3radio:"),
            metadata: docs::item_metadata(
                "-1",
                "Sim Radio",
                "object.item.audioItem.audioBroadcast",
                "SA_RINCON65031_",
            ),
            kind: "instantPlay",
            description: "Custom Station".to_string(),
        },
    ];
    Household {
        id: format!("SIM_HOUSEHOLD_S{sw_gen}"),
        sw_gen,
        params,
        favorites,
    }
}

/// Running virtual households. Dropping it stops every listener.
#[derive(Debug)]
pub struct SimHandle {
    state: Arc<Mutex<State>>,
    players: Vec<SimPlayerInfo>,
    listeners: Vec<server::Listener>,
    notifier: Option<JoinHandle<()>>,
    responder: ssdp::Responder,
    clock: SimClock,
}

impl SimHandle {
    /// Every virtual player, in household then spec order.
    #[must_use]
    pub fn players(&self) -> &[SimPlayerInfo] {
        &self.players
    }

    /// The player in `room` (case-insensitive).
    #[must_use]
    pub fn player(&self, room: &str) -> Option<&SimPlayerInfo> {
        self.players
            .iter()
            .find(|p| p.room.eq_ignore_ascii_case(room))
    }

    /// A [`Transport`] for every player, routed by the address each one
    /// advertises, the way the real LAN transport routes by IP.
    /// It follows address changes ([`Self::change_address`]).
    /// Its [`Transport::ssdp_search`] asks the simulator's unicast SSDP
    /// responder ([`Self::ssdp_addr`]).
    #[must_use]
    pub fn lan(&self) -> SimLan {
        SimLan {
            state: Arc::clone(&self.state),
            ssdp: self.responder.addr,
        }
    }

    /// The unicast SSDP responder: send it an `M-SEARCH` for ZonePlayers and
    /// every powered-on player answers with its advertised LOCATION.
    #[must_use]
    pub fn ssdp_addr(&self) -> SocketAddr {
        self.responder.addr
    }

    /// A seeds file (`fsonos --seeds` / `FSONOS_SEEDS`) listing every
    /// player's advertised address, as the owner would list real players.
    /// Reach those addresses through [`Self::lan`]. Reflects address changes.
    #[must_use]
    pub fn seeds_toml(&self) -> String {
        let addrs: Vec<String> = self
            .state
            .lock()
            .map(|s| s.players.iter().map(|p| format!("\"{}\"", p.ip)).collect())
            .unwrap_or_default();
        format!(
            "# fsonos-sim players: advertised addresses (port 1400), reached through SimLan.\n\
             players = [{}]\n",
            addrs.join(", ")
        )
    }

    /// A [`Transport`] that reaches the player in `room`.
    #[must_use]
    pub fn transport(&self, room: &str) -> Option<SimTransport> {
        self.player(room).map(|p| SimTransport {
            base_url: p.base_url.clone(),
        })
    }

    /// The Spotify render parameters the household of generation `sw_gen`
    /// (1 or 2) accepts: what the client must learn from its favorites.
    #[must_use]
    pub fn render_params(&self, sw_gen: u8) -> Option<SpotifyRenderParams> {
        let state = self.state.lock().ok()?;
        state
            .households
            .iter()
            .find(|h| h.sw_gen == sw_gen)
            .map(|h| h.params.clone())
    }

    /// Keep only the favorites of the household of generation `sw_gen` whose
    /// URI satisfies `keep`: e.g. drop its Spotify items to model a household
    /// where Spotify was never added to My Sonos.
    pub fn retain_favorites(
        &self,
        sw_gen: u8,
        keep: impl Fn(&str) -> bool,
    ) -> Result<(), SimError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| SimError::Invalid("state lock poisoned".into()))?;
        let household = state
            .households
            .iter_mut()
            .find(|h| h.sw_gen == sw_gen)
            .ok_or_else(|| SimError::Invalid(format!("no S{sw_gen} household")))?;
        household.favorites.retain(|f| keep(&f.uri));
        Ok(())
    }

    /// Move simulated time on by `by`, as the players live it: a queue track
    /// that ends starts the next (the queue ends STOPPED, back on its first
    /// track), a URI that plays out stops, and a sleep timer that runs out
    /// pauses its group, each at its own moment and evented then.
    /// [`SimClock::advance`] only moves the clock: changes then happen at
    /// the next request, still in order.
    pub fn advance(&self, by: Duration) {
        let Ok(mut s) = self.state.lock() else {
            return;
        };
        let target = self
            .clock
            .now_ms()
            .saturating_add(u64::try_from(by.as_millis()).unwrap_or(u64::MAX));
        for _ in 0..100_000 {
            let Some(at) = s.next_change().filter(|at| *at <= target) else {
                break;
            };
            let now = self.clock.now_ms();
            self.clock
                .advance(Duration::from_millis(at.saturating_sub(now)));
            s.settle_at(at);
        }
        let now = self.clock.now_ms();
        self.clock
            .advance(Duration::from_millis(target.saturating_sub(now)));
    }

    /// Every media fetch finished so far, oldest first.
    #[must_use]
    pub fn fetch_log(&self) -> Vec<FetchLogEntry> {
        self.state
            .lock()
            .map(|s| s.fetch_log.clone())
            .unwrap_or_default()
    }

    /// Wait until every media fetch players have started is finished, for
    /// at most `timeout`. Whether they all finished.
    #[must_use]
    pub fn wait_for_fetches(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            let idle = self
                .state
                .lock()
                .is_ok_and(|s| s.fetches_in_flight == 0 && s.pending_fetches.is_empty());
            if idle {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Every SOAP request answered so far, oldest first.
    #[must_use]
    pub fn soap_log(&self) -> Vec<SoapLogEntry> {
        self.state.lock().map(|s| s.log.clone()).unwrap_or_default()
    }

    /// The simulator's clock: advance it to move playback positions.
    #[must_use]
    pub fn clock(&self) -> &SimClock {
        &self.clock
    }

    /// Stop every listener and wait for its thread.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        for l in &self.listeners {
            l.shutdown.trigger_immediate();
        }
        for l in self.listeners.drain(..) {
            let _ = l.thread.join();
        }
        self.responder.stop();
        // The last sender goes with the state's; the notifier drains and ends.
        if let Ok(mut s) = self.state.lock() {
            s.notifier = None;
        }
        if let Some(notifier) = self.notifier.take() {
            let _ = notifier.join();
        }
    }

    /// Every GENA event so far, oldest first.
    #[must_use]
    pub fn gena_log(&self) -> Vec<GenaLogEntry> {
        self.state
            .lock()
            .map(|s| s.gena_log.clone())
            .unwrap_or_default()
    }

    /// Run `f` on the player named by `who` (a room, its visible player, or a
    /// player id), then issue any NOTIFYs the change causes.
    fn with_player<R>(
        &self,
        who: &str,
        f: impl FnOnce(&mut State, usize) -> Result<R, SimError>,
    ) -> Result<R, SimError> {
        let mut s = self
            .state
            .lock()
            .map_err(|_| SimError::Invalid("state lock poisoned".into()))?;
        let p = s
            .players
            .iter()
            .position(|p| p.uuid == who)
            .or_else(|| {
                s.players
                    .iter()
                    .position(|p| p.pair_primary.is_none() && p.room.eq_ignore_ascii_case(who))
            })
            .ok_or_else(|| SimError::UnknownPlayer(who.to_string()))?;
        let out = f(&mut s, p)?;
        s.flush_events();
        Ok(out)
    }

    /// Delay every answer from `who` by `latency`.
    pub fn set_latency(&self, who: &str, latency: Duration) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.players[p].faults.latency = latency;
            Ok(())
        })
    }

    /// Swallow NOTIFYs `who` would send (`None` delivers them again).
    pub fn drop_notifies(&self, who: &str, drop: Option<NotifyDrop>) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.players[p].faults.drop_notifies = drop;
            Ok(())
        })
    }

    /// Make `action` on `who` fail with UPnP error `code` until
    /// [`Self::clear_faults`].
    pub fn upnp_fault(&self, who: &str, action: &str, code: u16) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.players[p].faults.upnp.push((action.to_string(), code));
            Ok(())
        })
    }

    /// Make `who` slow to join: a join (`SetAVTransportURI` to `x-rincon:`)
    /// is answered at once but shows in the topology only `lag` later, as on
    /// a busy real network.
    pub fn join_lag(&self, who: &str, lag: Duration) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.players[p].faults.join_lag = lag;
            Ok(())
        })
    }

    /// Remove every injected fault from `who`.
    pub fn clear_faults(&self, who: &str) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.players[p].faults = model::Faults::default();
            Ok(())
        })
    }

    /// Reboot `who`: its subscriptions are gone (renewing one gets 412), its
    /// BootSeq goes up, and it answers nothing but 503 for `down_for`.
    pub fn reboot(&self, who: &str, down_for: Duration) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            s.drop_subscriptions(p);
            s.players[p].boot_seq += 1;
            s.players[p].faults.unreachable_until = Some(Instant::now() + down_for);
            s.log_gena(p, "", GenaEvent::Rebooted);
            Ok(())
        })
    }

    /// Give `who` a new address, as a DHCP lease change would: its Location,
    /// description and the topology show it, [`SimLan`] reaches it there,
    /// and the old address no longer answers. Returns the new address.
    pub fn change_address(&mut self, who: &str) -> Result<IpAddr, SimError> {
        let (uuid, to) = self.with_player(who, |s, p| {
            let taken: Vec<u8> = s.players.iter().map(|o| o.ip.octets()[3]).collect();
            let host = (100..=254)
                .rev()
                .find(|h| !taken.contains(h))
                .ok_or_else(|| SimError::Invalid("no free virtual address".into()))?;
            let from = IpAddr::V4(s.players[p].ip);
            s.players[p].ip = Ipv4Addr::new(192, 0, 2, host);
            let to = IpAddr::V4(s.players[p].ip);
            s.log_gena(p, "", GenaEvent::AddressChanged { from, to });
            Ok((s.players[p].uuid.clone(), to))
        })?;
        if let Some(info) = self.players.iter_mut().find(|i| i.uuid == uuid) {
            info.ip = to;
        }
        Ok(to)
    }

    /// The coordinator of `who`'s group hands it to another visible member,
    /// as Sonos does when a coordinator drops out; playback (transport and
    /// queue) moves to the new coordinator.
    pub fn reelect_coordinator(&self, who: &str) -> Result<(), SimError> {
        self.with_player(who, |s, p| s.reelect(s.players[p].coordinator))
    }

    /// Power `who` off (`true`) or back on. Off: it answers 503, sends no
    /// events, loses its subscriptions, leaves its group (the group
    /// re-elects if it led) and shows under `VanishedDevices`. On: it boots
    /// (BootSeq up) into a group of its own.
    pub fn set_offline(&self, who: &str, offline: bool) -> Result<(), SimError> {
        self.with_player(who, |s, p| {
            if s.players[p].offline == offline {
                return Ok(());
            }
            if offline {
                if s.players[p].coordinator == p && s.members(p).len() > 1 {
                    let _ = s.reelect(p);
                }
                s.make_standalone(p);
                s.drop_subscriptions(p);
                s.players[p].offline = true;
                s.log_gena(p, "", GenaEvent::WentOffline);
            } else {
                s.players[p].offline = false;
                s.players[p].boot_seq += 1;
                s.log_gena(p, "", GenaEvent::CameOnline);
            }
            Ok(())
        })
    }
}

impl Drop for SimHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A [`Transport`] bound to one virtual player (the `host` argument is
/// ignored: every virtual player is on `127.0.0.1`). Call it from synchronous
/// code; each request runs on a short-lived asupersync runtime.
#[derive(Debug, Clone)]
pub struct SimTransport {
    base_url: String,
}

impl SimTransport {
    /// `127.0.0.1`, for APIs that want a host.
    #[must_use]
    pub fn ip(&self) -> IpAddr {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    }

    /// `http://127.0.0.1:<port>`.
    #[must_use]
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    fn request(
        &self,
        path: &str,
        soap_action: Option<&str>,
        body: Option<&str>,
    ) -> Result<String, ProtoError> {
        let url = format!("{}{path}", self.base_url);
        let network = |detail: String| ProtoError::Network {
            target: url.clone(),
            detail,
        };
        let runtime = create_reactor()
            .and_then(|reactor| {
                RuntimeBuilder::current_thread()
                    .with_reactor(reactor)
                    .build()
                    .map_err(std::io::Error::other)
            })
            .map_err(|e| network(e.to_string()))?;
        let response = runtime.block_on(async {
            let cx = Cx::current().ok_or_else(|| network("no runtime context".into()))?;
            let client = Client::default_for_runtime(&cx);
            let request = match (soap_action, body) {
                (Some(action), Some(body)) => client
                    .post(url.clone())
                    .header("Content-Type", "text/xml; charset=\"utf-8\"")
                    .header("SOAPACTION", action)
                    .body(body.as_bytes().to_vec()),
                _ => client.get(url.clone()),
            };
            request.send(&cx).await.map_err(|e| network(e.to_string()))
        })?;
        match response.status {
            200 | 500 => Ok(String::from_utf8_lossy(&response.body).into_owned()),
            status => Err(network(format!("HTTP {status}"))),
        }
    }
}

impl Transport for SimTransport {
    fn soap_post(
        &self,
        _host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.request(control_path, Some(soap_action), Some(body))
    }

    /// GET `url`'s path from this player (the host part is ignored).
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        let path = url
            .split_once("://")
            .and_then(|(_, rest)| rest.find('/').map(|i| &rest[i..]))
            .unwrap_or(url);
        self.request(path, None, None)
    }
}

/// A [`Transport`] for all virtual players: requests go to whichever player
/// advertises the `host` (or URL host) asked for. Lets IP-addressed client
/// code (topology folding, control orchestration) run unchanged against the
/// simulator. Call it from synchronous code.
#[derive(Debug, Clone)]
pub struct SimLan {
    state: Arc<Mutex<State>>,
    ssdp: SocketAddr,
}

impl SimLan {
    fn route(&self, host: IpAddr) -> Result<SimTransport, ProtoError> {
        let port = self.state.lock().ok().and_then(|s| {
            s.players
                .iter()
                .find(|p| IpAddr::V4(p.ip) == host)
                .map(|p| p.port)
        });
        port.map(|port| SimTransport {
            base_url: format!("http://127.0.0.1:{port}"),
        })
        .ok_or_else(|| ProtoError::Network {
            target: host.to_string(),
            detail: "no virtual player advertises this address".into(),
        })
    }
}

impl Transport for SimLan {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        self.route(host)?
            .soap_post(host, control_path, soap_action, body)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        let host = fsonos_proto::topology::host_of_location(url)
            .ok_or_else(|| ProtoError::Malformed(format!("no host in {url}")))?;
        self.route(host)?.http_get(url)
    }

    /// Send an `M-SEARCH` to the simulator's unicast responder and collect
    /// the replies that arrive within `wait`, one per player.
    fn ssdp_search(&self, mx_secs: u8, wait: Duration) -> Result<Vec<Advert>, ProtoError> {
        let network = |e: std::io::Error| ProtoError::Network {
            target: self.ssdp.to_string(),
            detail: e.to_string(),
        };
        let socket = UdpSocket::bind("127.0.0.1:0").map_err(network)?;
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .map_err(network)?;
        socket
            .send_to(m_search(mx_secs).as_bytes(), self.ssdp)
            .map_err(network)?;
        let deadline = Instant::now() + wait;
        let mut adverts: Vec<Advert> = Vec::new();
        let mut buf = [0u8; 2048];
        while Instant::now() < deadline {
            if let Ok((n, _)) = socket.recv_from(&mut buf)
                && let Some(advert) = parse_response(&buf[..n])
                && !adverts.iter().any(|a| a.location == advert.location)
            {
                adverts.push(advert);
            }
        }
        Ok(adverts)
    }
}
