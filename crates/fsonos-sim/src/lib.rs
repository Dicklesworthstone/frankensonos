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

mod docs;
mod model;
mod server;

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_proto::{ProtoError, Transport};
use model::{Favorite, Household, State};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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

#[derive(Debug, thiserror::Error)]
pub enum SimError {
    #[error("cannot start the simulator: {0}")]
    Start(String),
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
        let state = Arc::new(Mutex::new(state));
        let count = state.lock().map_or(0, |s| s.players.len());
        let mut handle = SimHandle {
            state: Arc::clone(&state),
            players: Vec::with_capacity(count),
            listeners: Vec::with_capacity(count),
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
    #[must_use]
    pub fn lan(&self) -> SimLan {
        SimLan {
            routes: self
                .players
                .iter()
                .map(|p| (p.ip, p.base_url.clone()))
                .collect(),
        }
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
    routes: Vec<(IpAddr, String)>,
}

impl SimLan {
    fn route(&self, host: IpAddr) -> Result<SimTransport, ProtoError> {
        self.routes
            .iter()
            .find(|(ip, _)| *ip == host)
            .map(|(_, base_url)| SimTransport {
                base_url: base_url.clone(),
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
}
