//! The virtual households' state and the SOAP actions that change it. Pure:
//! the server parses requests, calls [`State::invoke`], and renders the result.
//!
//! Faults follow what real players were observed to return (docs/PROTOCOL.md
//! §3): 714 for a URI scheme the service will not render (a raw `spotify:`
//! URI), 800 for a Spotify item whose `desc` / `sid` / `sn` do not match the
//! household's linked account, and 800 for a coordinator verb sent to a group
//! member. Standard UPnP codes cover the rest: 401 unknown action, 402 bad
//! arguments, 701 transition not available, 711 illegal seek target, 701 on
//! `Browse` of an object that does not exist.

use crate::docs::{self, ServiceDef};
use crate::{SimClock, SimModel, SoapLogEntry};
use fsonos_proto::didl::{SpotifyRenderParams, parse_didl};

/// A UPnP error code returned in a SOAP fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Fault(pub u16);

pub(crate) const INVALID_ACTION: Fault = Fault(401);
pub(crate) const INVALID_ARGS: Fault = Fault(402);
pub(crate) const TRANSITION_NOT_AVAILABLE: Fault = Fault(701);
pub(crate) const ILLEGAL_SEEK_TARGET: Fault = Fault(711);
pub(crate) const ILLEGAL_MIME_TYPE: Fault = Fault(714);
pub(crate) const SONOS_FAILURE: Fault = Fault(800);

/// Out-arguments of a successful action, in wire order.
pub(crate) type Out = Vec<(&'static str, String)>;

/// In-arguments of an action.
pub(crate) struct Args<'a>(pub &'a [(String, String)]);

impl Args<'_> {
    fn get(&self, name: &str) -> Result<&str, Fault> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
            .ok_or(INVALID_ARGS)
    }

    fn num<T: std::str::FromStr>(&self, name: &str) -> Result<T, Fault> {
        self.get(name)?.trim().parse().map_err(|_| INVALID_ARGS)
    }
}

/// A favorite in a household's `FV:2`.
#[derive(Debug, Clone)]
pub(crate) struct Favorite {
    pub title: String,
    pub uri: String,
    pub protocol_info: &'static str,
    /// The DIDL the favorite is replayed with (`r:resMD`).
    pub metadata: String,
    /// `instantPlay` or `shortcut`.
    pub kind: &'static str,
    pub description: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Household {
    pub id: String,
    pub sw_gen: u8,
    pub params: SpotifyRenderParams,
    pub favorites: Vec<Favorite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TransportState {
    Stopped,
    Playing,
    PausedPlayback,
}

impl TransportState {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "STOPPED",
            Self::Playing => "PLAYING",
            Self::PausedPlayback => "PAUSED_PLAYBACK",
        }
    }
}

/// What the transport plays from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Nothing,
    /// `x-rincon-queue:<own uuid>#0`.
    Queue,
    /// A single URI (a stream or one track).
    Uri,
}

#[derive(Debug, Clone)]
pub(crate) struct QueueItem {
    pub uri: String,
    pub metadata: String,
    pub title: String,
    pub creator: Option<String>,
    pub album: Option<String>,
    pub duration_ms: u64,
}

/// A coordinator's transport.
#[derive(Debug, Clone)]
pub(crate) struct Transport {
    pub state: TransportState,
    pub source: Source,
    pub uri: String,
    pub uri_metadata: String,
    /// Length of a URI source, from its DIDL `res duration` (0: unknown,
    /// plays forever).
    pub uri_duration_ms: u64,
    pub queue: Vec<QueueItem>,
    /// 1-based queue position; 0 when the queue is not the source or empty.
    pub track: usize,
    /// Position when the transport last started or stopped moving.
    pub position_ms: u64,
    /// Clock time playback (re)started, while PLAYING.
    pub playing_since: Option<u64>,
    pub queue_update_id: u32,
    /// Clock time the sleep timer pauses the group at.
    pub sleep_at: Option<u64>,
    /// Bumped each time the sleep timer is set or cleared.
    pub sleep_generation: u32,
    /// The current URI's media was fetched (or is being fetched).
    pub fetched: bool,
}

impl Transport {
    pub(crate) fn new() -> Self {
        Self {
            state: TransportState::Stopped,
            source: Source::Nothing,
            uri: String::new(),
            uri_metadata: String::new(),
            uri_duration_ms: 0,
            queue: Vec::new(),
            track: 0,
            position_ms: 0,
            playing_since: None,
            queue_update_id: 0,
            sleep_at: None,
            sleep_generation: 0,
            fetched: false,
        }
    }

    pub(crate) fn current(&self) -> Option<&QueueItem> {
        (self.source == Source::Queue && self.track > 0)
            .then(|| self.queue.get(self.track - 1))
            .flatten()
    }

    pub(crate) fn duration_ms(&self) -> u64 {
        match self.source {
            Source::Queue => self.current().map_or(0, |q| q.duration_ms),
            Source::Uri => self.uri_duration_ms,
            Source::Nothing => 0,
        }
    }

    fn position(&self, now: u64) -> u64 {
        let moved = self.playing_since.map_or(0, |t| now.saturating_sub(t));
        let pos = self.position_ms + moved;
        match self.duration_ms() {
            0 => pos,
            d => pos.min(d),
        }
    }

    /// The clock time what is playing ends (`None` when it is not playing or
    /// has no known length).
    fn ends_at(&self) -> Option<u64> {
        let since = self.playing_since?;
        let length = self.duration_ms();
        (self.state == TransportState::Playing && length > 0)
            .then(|| since + length.saturating_sub(self.position_ms))
    }

    /// Carry playback on to clock time `now`: a queue track that has ended
    /// starts the next one, the queue ends STOPPED back on its first track,
    /// and a URI that has played out stops back at its start. Whether
    /// anything changed.
    fn play_on(&mut self, now: u64) -> bool {
        let mut changed = false;
        while let Some(end) = self.ends_at().filter(|end| *end <= now) {
            changed = true;
            if self.source == Source::Queue && self.track < self.queue.len() {
                self.track += 1;
                self.position_ms = 0;
                self.playing_since = Some(end);
            } else {
                if self.source == Source::Queue {
                    self.track = 1;
                }
                self.state = TransportState::Stopped;
                self.position_ms = 0;
                self.playing_since = None;
            }
        }
        changed
    }

    fn set_state(&mut self, to: TransportState, now: u64) {
        self.position_ms = self.position(now);
        self.playing_since = (to == TransportState::Playing).then_some(now);
        self.state = to;
    }

    fn go_to_track(&mut self, track: usize, now: u64) {
        self.track = track;
        self.position_ms = 0;
        if self.state == TransportState::Playing {
            self.playing_since = Some(now);
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct Player {
    pub uuid: String,
    pub room: String,
    pub model: SimModel,
    pub household: usize,
    /// The address the player advertises (TEST-NET-1, `192.0.2.N`): in its
    /// Location, its description, and the topology, on port 1400 like a real
    /// player. [`crate::SimLan`] maps it to the real loopback socket.
    pub ip: std::net::Ipv4Addr,
    pub port: u16,
    /// For the hidden (RF) half of a stereo pair: its visible primary.
    pub pair_primary: Option<usize>,
    /// Index of this player's group coordinator (itself when it leads).
    pub coordinator: usize,
    pub group_id: String,
    pub boot_seq: u32,
    pub volume: u8,
    pub mute: bool,
    pub transport: Transport,
    /// Powered off: answers nothing, sends no events, shows as vanished.
    pub offline: bool,
    pub faults: Faults,
}

/// Failures injected into one player (see the `SimHandle` fault API).
#[derive(Debug, Clone, Default)]
pub(crate) struct Faults {
    /// Added before every answer.
    pub latency: std::time::Duration,
    /// NOTIFYs to swallow instead of delivering.
    pub drop_notifies: Option<crate::NotifyDrop>,
    /// `(action, UPnP error code)`: these actions fail until cleared.
    pub upnp: Vec<(String, u16)>,
    /// Unreachable (rebooting) until this instant.
    pub unreachable_until: Option<std::time::Instant>,
    /// How long a join (`x-rincon:`) takes to show in the topology.
    pub join_lag: std::time::Duration,
    /// A join waiting out `join_lag`: (the player joined, when it lands).
    pub pending_join: Option<(usize, std::time::Instant)>,
}

impl Player {
    /// The MAC the synthetic id encodes, `00:0E:58:A0:..:..`.
    pub(crate) fn mac(&self) -> String {
        let hex = &self.uuid["RINCON_".len().."RINCON_".len() + 12];
        hex.as_bytes()
            .chunks(2)
            .map(|c| String::from_utf8_lossy(c).into_owned())
            .collect::<Vec<_>>()
            .join(":")
    }

    pub(crate) fn location(&self) -> String {
        format!("http://{}:1400/xml/device_description.xml", self.ip)
    }
}

/// Everything the simulator knows.
#[derive(Debug)]
pub(crate) struct State {
    pub households: Vec<Household>,
    pub players: Vec<Player>,
    pub clock: SimClock,
    pub log: Vec<SoapLogEntry>,
    pub subscriptions: Vec<crate::gena::Subscription>,
    pub gena_log: Vec<crate::GenaLogEntry>,
    /// Where NOTIFYs go to be delivered (`None` once shut down).
    pub notifier: Option<std::sync::mpsc::Sender<crate::gena::Outgoing>>,
    pub next_sid: u64,
    /// xorshift state for probabilistic NOTIFY drops (fixed seed).
    pub rng: u64,
    next_group: u64,
    /// Media fetches to start once the request that asked for them is
    /// answered: (player, URL).
    pub pending_fetches: Vec<(usize, String)>,
    pub fetches_in_flight: usize,
    pub fetch_log: Vec<crate::FetchLogEntry>,
}

impl State {
    pub(crate) fn new(households: Vec<Household>, clock: SimClock) -> Self {
        Self {
            households,
            players: Vec::new(),
            clock,
            log: Vec::new(),
            subscriptions: Vec::new(),
            gena_log: Vec::new(),
            notifier: None,
            next_sid: 1,
            rng: 0x9E37_79B9_7F4A_7C15,
            next_group: 1,
            pending_fetches: Vec::new(),
            fetches_in_flight: 0,
            fetch_log: Vec::new(),
        }
    }

    /// Add a player as the coordinator of its own group.
    pub(crate) fn add_player(&mut self, room: &str, model: SimModel, household: usize) -> usize {
        let index = self.players.len();
        let uuid = format!("RINCON_000E58A0{:04X}01400", index + 1);
        let group_id = format!("{uuid}:{}", u8::from(model.is_renderer()));
        let host = u8::try_from(10 + index).expect("at most 245 virtual players");
        self.players.push(Player {
            uuid,
            room: room.to_string(),
            model,
            household,
            ip: std::net::Ipv4Addr::new(192, 0, 2, host),
            port: 0,
            pair_primary: None,
            coordinator: index,
            group_id,
            boot_seq: 1,
            volume: 20,
            mute: false,
            transport: Transport::new(),
            offline: false,
            faults: Faults::default(),
        });
        index
    }

    /// Add a stereo pair: a visible primary (LF) and its hidden secondary
    /// (RF), one room, one group. Returns the primary.
    pub(crate) fn add_pair(&mut self, room: &str, model: SimModel, household: usize) -> usize {
        let primary = self.add_player(room, model, household);
        let secondary = self.add_player(room, model, household);
        self.players[secondary].pair_primary = Some(primary);
        self.sync_pairs();
        primary
    }

    /// The hidden half of `p`'s stereo pair, if `p` is a pair's primary.
    pub(crate) fn secondary_of(&self, p: usize) -> Option<usize> {
        self.players.iter().position(|o| o.pair_primary == Some(p))
    }

    /// Keep every pair's hidden half in its primary's group.
    pub(crate) fn sync_pairs(&mut self) {
        for i in 0..self.players.len() {
            if let Some(primary) = self.players[i].pair_primary {
                self.players[i].coordinator = self.players[primary].coordinator;
                self.players[i].group_id = self.players[primary].group_id.clone();
            }
        }
    }

    fn require_not_secondary(&self, p: usize) -> Result<(), Fault> {
        if self.players[p].pair_primary.is_some() {
            Err(SONOS_FAILURE)
        } else {
            Ok(())
        }
    }

    /// Coordinators of household `h`, in player order.
    pub(crate) fn coordinators(&self, h: usize) -> Vec<usize> {
        (0..self.players.len())
            .filter(|&i| self.players[i].household == h && self.players[i].coordinator == i)
            .collect()
    }

    /// Members of the group `coord` leads, coordinator first.
    pub(crate) fn members(&self, coord: usize) -> Vec<usize> {
        let mut m = vec![coord];
        m.extend(
            (0..self.players.len()).filter(|&i| i != coord && self.players[i].coordinator == coord),
        );
        m
    }

    fn new_group_id(&mut self, coord: usize) -> String {
        self.next_group += 1;
        format!("{}:{}", self.players[coord].uuid, self.next_group)
    }

    /// Take `p` (and its pair's hidden half) out of its group into a group
    /// of its own. If it led others, the first visible one becomes their
    /// coordinator (keeping the group id).
    pub(crate) fn make_standalone(&mut self, p: usize) {
        let own = self.secondary_of(p);
        let led: Vec<usize> = self
            .members(p)
            .into_iter()
            .skip(1)
            .filter(|&m| Some(m) != own)
            .collect();
        if let Some(&heir) = led
            .iter()
            .find(|&&m| self.players[m].pair_primary.is_none())
        {
            let group_id = self.players[p].group_id.clone();
            for &m in &led {
                self.players[m].coordinator = heir;
            }
            self.players[heir].group_id = group_id;
        }
        self.players[p].coordinator = p;
        self.players[p].group_id = self.new_group_id(p);
        self.sync_pairs();
    }

    /// Hand the group `coord` leads to its first other visible, online
    /// member; playback (transport and queue) moves with it.
    pub(crate) fn reelect(&mut self, coord: usize) -> Result<(), crate::SimError> {
        let heir = self
            .members(coord)
            .into_iter()
            .find(|&m| {
                m != coord && self.players[m].pair_primary.is_none() && !self.players[m].offline
            })
            .ok_or_else(|| {
                crate::SimError::Invalid(format!(
                    "{} leads no other member to take over",
                    self.players[coord].room
                ))
            })?;
        self.hand_over(coord, heir);
        let to = self.players[heir].uuid.clone();
        self.log_gena(coord, "", crate::GenaEvent::CoordinatorReelected { to });
        Ok(())
    }

    /// Make `heir` coordinator of the group `coord` leads; playback
    /// (transport and queue) moves with it and `coord` stays a member.
    pub(crate) fn hand_over(&mut self, coord: usize, heir: usize) {
        let mut transport = std::mem::replace(&mut self.players[coord].transport, Transport::new());
        if transport.source == Source::Queue {
            transport.uri = format!("x-rincon-queue:{}#0", self.players[heir].uuid);
        }
        self.players[heir].transport = transport;
        let group_id = self.players[coord].group_id.clone();
        for m in self.members(coord) {
            self.players[m].coordinator = heir;
        }
        self.players[heir].group_id = group_id;
        self.sync_pairs();
    }

    /// `DelegateGroupCoordinationTo`: hand the group `p` leads to its member
    /// `NewCoordinator` (playback moves with it); with `RejoinGroup` 0, `p`
    /// then leaves for a group of its own.
    fn delegate(&mut self, p: usize, args: &Args<'_>) -> Result<Out, Fault> {
        self.require_coordinator(p)?;
        let to = self.find(args.get("NewCoordinator")?).ok_or(INVALID_ARGS)?;
        let rejoin = bool_arg(args, "RejoinGroup")?;
        if to == p || self.players[to].coordinator != p || self.players[to].pair_primary.is_some() {
            return Err(SONOS_FAILURE);
        }
        self.hand_over(p, to);
        if !rejoin {
            self.make_standalone(p);
        }
        Ok(Vec::new())
    }

    /// `SetAVTransportURI x-rincon:<target>`: join `target`'s group, now or,
    /// with a join lag, later (see [`Self::settle_joins`]).
    fn join_uri(&mut self, p: usize, target: &str) -> Result<Out, Fault> {
        let t = self.find(target).ok_or(SONOS_FAILURE)?;
        let lag = self.players[p].faults.join_lag;
        if lag.is_zero() {
            self.join(p, t)?;
        } else {
            self.players[p].faults.pending_join = Some((t, std::time::Instant::now() + lag));
        }
        Ok(Vec::new())
    }

    /// The group's sleep timer: set, cleared (an empty duration), or read.
    fn sleep_timer(
        &mut self,
        p: usize,
        action: &str,
        args: &Args<'_>,
        now: u64,
    ) -> Result<Out, Fault> {
        self.require_coordinator(p)?;
        let t = &mut self.players[p].transport;
        if action == "ConfigureSleepTimer" {
            let duration = args.get("NewSleepTimerDuration")?;
            t.sleep_at = if duration.is_empty() {
                None
            } else {
                Some(now + parse_hms(duration).ok_or(INVALID_ARGS)?)
            };
            t.sleep_generation += 1;
            return Ok(Vec::new());
        }
        let remaining = t
            .sleep_at
            .map_or_else(String::new, |at| docs::hms(at.saturating_sub(now)));
        Ok(vec![
            ("RemainingSleepTimerDuration", remaining),
            (
                "CurrentSleepTimerGeneration",
                t.sleep_generation.to_string(),
            ),
        ])
    }

    /// The next clock time something changes by itself: a track or URI
    /// ending, or a sleep timer running out.
    pub(crate) fn next_change(&self) -> Option<u64> {
        self.players
            .iter()
            .flat_map(|p| [p.transport.ends_at(), p.transport.sleep_at])
            .flatten()
            .min()
    }

    /// Bring every player up to the clock: whatever came due since the last
    /// look happens in order, each change evented at its own moment.
    pub(crate) fn settle(&mut self) {
        let now = self.clock.now_ms();
        for _ in 0..100_000 {
            match self.next_change().filter(|at| *at <= now) {
                Some(at) => self.settle_at(at),
                None => break,
            }
        }
    }

    /// Everything due at clock time `at`: playback carries on (see
    /// `Transport::play_on`) and a sleep timer that has run out pauses its
    /// group.
    pub(crate) fn settle_at(&mut self, at: u64) {
        let mut changed = false;
        for player in &mut self.players {
            let t = &mut player.transport;
            changed |= t.play_on(at);
            if t.sleep_at.is_some_and(|due| due <= at) {
                t.sleep_at = None;
                t.sleep_generation += 1;
                if t.state == TransportState::Playing {
                    t.set_state(TransportState::PausedPlayback, at);
                }
                changed = true;
            }
        }
        if changed {
            self.flush_events();
        }
    }

    /// A media fetch started by `Play` has finished. A failed one stops the
    /// player if it is still on that URI; a WAV teaches it the length.
    pub(crate) fn media_fetched(&mut self, p: usize, url: &str, outcome: crate::fetch::Fetched) {
        self.fetches_in_flight = self.fetches_in_flight.saturating_sub(1);
        let now = self.clock.now_ms();
        let ok = matches!(outcome.status, Ok(status) if status < 400);
        let t = &mut self.players[p].transport;
        if t.source == Source::Uri && t.uri == url {
            if !ok && t.state == TransportState::Playing {
                t.set_state(TransportState::Stopped, now);
                t.position_ms = 0;
            }
            if let Some(length) = outcome.wav_duration_ms.filter(|_| ok) {
                t.uri_duration_ms = length;
            }
        }
        let entry = crate::FetchLogEntry {
            player: self.players[p].uuid.clone(),
            room: self.players[p].room.clone(),
            url: url.to_string(),
            result: outcome.status,
            bytes: outcome.bytes,
            wav_duration_ms: outcome.wav_duration_ms,
            at_ms: now,
        };
        self.fetch_log.push(entry);
        self.flush_events();
    }

    /// Carry out the lagging joins whose time has come.
    pub(crate) fn settle_joins(&mut self, now: std::time::Instant) {
        let due: Vec<(usize, usize)> = (0..self.players.len())
            .filter_map(|p| match self.players[p].faults.pending_join {
                Some((target, at)) if at <= now => Some((p, target)),
                _ => None,
            })
            .collect();
        if due.is_empty() {
            return;
        }
        for (p, target) in due {
            self.players[p].faults.pending_join = None;
            // A join that has become impossible meanwhile just never lands.
            let _ = self.join(p, target);
        }
        self.flush_events();
    }

    /// Join `p` to the group that `target` belongs to.
    fn join(&mut self, p: usize, target: usize) -> Result<(), Fault> {
        let coord = self.players[target].coordinator;
        if self.players[target].household != self.players[p].household
            || !self.players[coord].model.is_renderer()
        {
            return Err(SONOS_FAILURE);
        }
        if coord == p || self.players[p].coordinator == coord {
            return Ok(());
        }
        self.make_standalone(p);
        self.players[p].coordinator = coord;
        let group_id = self.players[coord].group_id.clone();
        self.players[p].group_id = group_id;
        self.sync_pairs();
        Ok(())
    }

    fn find(&self, uuid: &str) -> Option<usize> {
        self.players.iter().position(|p| p.uuid == uuid)
    }

    fn require_coordinator(&self, p: usize) -> Result<(), Fault> {
        if self.players[p].coordinator == p {
            Ok(())
        } else {
            Err(SONOS_FAILURE)
        }
    }

    /// Run `action` of `service` on player `p`.
    pub(crate) fn invoke(
        &mut self,
        p: usize,
        service: &ServiceDef,
        action: &str,
        args: &Args<'_>,
    ) -> Result<Out, Fault> {
        match service.name {
            "AVTransport" => self.av_transport(p, action, args),
            "RenderingControl" => self.rendering_control(p, action, args),
            "GroupRenderingControl" => self.group_rendering_control(p, action, args),
            "ZoneGroupTopology" => self.zone_group_topology(p, action),
            "ContentDirectory" => self.content_directory(p, action, args),
            "DeviceProperties" => self.device_properties(p, action),
            _ => Err(INVALID_ACTION),
        }
    }

    fn av_transport(&mut self, p: usize, action: &str, args: &Args<'_>) -> Result<Out, Fault> {
        let now = self.clock.now_ms();
        match action {
            "SetAVTransportURI"
            | "AddURIToQueue"
            | "RemoveTrackFromQueue"
            | "RemoveAllTracksFromQueue" => self.av_sources(p, action, args, now),
            "Play" | "Pause" | "Stop" => self.av_verbs(p, action, now),
            "Next" | "Previous" | "Seek" | "BecomeCoordinatorOfStandaloneGroup" => {
                self.av_navigation(p, action, args, now)
            }
            "DelegateGroupCoordinationTo" => self.delegate(p, args),
            "ConfigureSleepTimer" | "GetRemainingSleepTimerDuration" => {
                self.sleep_timer(p, action, args, now)
            }
            "GetTransportInfo" | "GetPositionInfo" | "GetMediaInfo" => {
                self.av_reads(p, action, now)
            }
            _ => Err(INVALID_ACTION),
        }
    }

    /// What the transport plays from: its URI and its queue.
    fn av_sources(
        &mut self,
        p: usize,
        action: &str,
        args: &Args<'_>,
        now: u64,
    ) -> Result<Out, Fault> {
        match action {
            "SetAVTransportURI" => {
                // The hidden half of a pair follows its primary.
                self.require_not_secondary(p)?;
                let uri = args.get("CurrentURI")?.to_string();
                let metadata = args.get("CurrentURIMetaData")?.to_string();
                if let Some(target) = uri.strip_prefix("x-rincon:") {
                    return self.join_uri(p, target);
                }
                // Any other source makes a member leave its group first.
                if self.players[p].coordinator != p {
                    self.make_standalone(p);
                }
                let own_queue = format!("x-rincon-queue:{}#0", self.players[p].uuid);
                let source = if uri == own_queue {
                    Source::Queue
                } else {
                    self.check_renderable(p, &uri, &metadata)?;
                    Source::Uri
                };
                let length = if source == Source::Uri {
                    didl_duration_ms(&metadata)
                } else {
                    0
                };
                let t = &mut self.players[p].transport;
                t.set_state(TransportState::Stopped, now);
                t.source = source;
                t.uri_duration_ms = length;
                t.fetched = false;
                t.uri = uri;
                t.uri_metadata = metadata;
                t.track = usize::from(source == Source::Queue && !t.queue.is_empty());
                t.position_ms = 0;
                Ok(Vec::new())
            }
            "AddURIToQueue" => {
                self.require_coordinator(p)?;
                let uri = args.get("EnqueuedURI")?.to_string();
                let metadata = args.get("EnqueuedURIMetaData")?.to_string();
                let desired: usize = args.num("DesiredFirstTrackNumberEnqueued")?;
                let as_next = args.get("EnqueueAsNext")? == "1";
                self.check_renderable(p, &uri, &metadata)?;
                let items = expand(&self.queued_uri(p, &uri), &metadata);
                let t = &mut self.players[p].transport;
                let at = if as_next && t.track > 0 {
                    t.track + 1
                } else if desired == 0 || desired > t.queue.len() + 1 {
                    t.queue.len() + 1
                } else {
                    desired
                };
                let added = items.len();
                for (i, item) in items.into_iter().enumerate() {
                    t.queue.insert(at - 1 + i, item);
                }
                if t.track >= at {
                    t.track += added;
                }
                t.queue_update_id += 1;
                Ok(vec![
                    ("FirstTrackNumberEnqueued", at.to_string()),
                    ("NumTracksAdded", added.to_string()),
                    ("NewQueueLength", t.queue.len().to_string()),
                ])
            }
            "RemoveTrackFromQueue" => {
                self.require_coordinator(p)?;
                let n: usize = args
                    .get("ObjectID")?
                    .strip_prefix("Q:0/")
                    .and_then(|n| n.parse().ok())
                    .ok_or(INVALID_ARGS)?;
                let t = &mut self.players[p].transport;
                if n == 0 || n > t.queue.len() {
                    return Err(INVALID_ARGS);
                }
                t.queue.remove(n - 1);
                if t.track > n || t.track > t.queue.len() {
                    t.track -= 1;
                }
                t.queue_update_id += 1;
                Ok(Vec::new())
            }
            "RemoveAllTracksFromQueue" => {
                self.require_coordinator(p)?;
                let t = &mut self.players[p].transport;
                t.queue.clear();
                t.queue_update_id += 1;
                if t.source == Source::Queue {
                    t.set_state(TransportState::Stopped, now);
                    t.track = 0;
                    t.position_ms = 0;
                }
                Ok(Vec::new())
            }
            _ => Err(INVALID_ACTION),
        }
    }

    /// Start, pause, and stop (coordinator only).
    fn av_verbs(&mut self, p: usize, action: &str, now: u64) -> Result<Out, Fault> {
        match action {
            "Play" => {
                self.require_coordinator(p)?;
                let t = &mut self.players[p].transport;
                let playable = match t.source {
                    Source::Nothing => false,
                    Source::Uri => true,
                    Source::Queue => t.current().is_some(),
                };
                if !playable {
                    return Err(TRANSITION_NOT_AVAILABLE);
                }
                if t.state != TransportState::Playing {
                    t.set_state(TransportState::Playing, now);
                }
                // A player fetches what it is told to play; here only
                // loopback http:// media is fetched (never the network).
                if t.source == Source::Uri && !t.fetched && crate::fetch::is_fetchable(&t.uri) {
                    t.fetched = true;
                    self.pending_fetches.push((p, t.uri.clone()));
                    self.fetches_in_flight += 1;
                }
                Ok(Vec::new())
            }
            "Pause" => {
                self.require_coordinator(p)?;
                let t = &mut self.players[p].transport;
                if t.state != TransportState::Playing {
                    return Err(TRANSITION_NOT_AVAILABLE);
                }
                t.set_state(TransportState::PausedPlayback, now);
                Ok(Vec::new())
            }
            "Stop" => {
                self.require_coordinator(p)?;
                let t = &mut self.players[p].transport;
                t.set_state(TransportState::Stopped, now);
                t.position_ms = 0;
                Ok(Vec::new())
            }
            _ => Err(INVALID_ACTION),
        }
    }

    /// Moving within the queue or the track, and leaving a group.
    fn av_navigation(
        &mut self,
        p: usize,
        action: &str,
        args: &Args<'_>,
        now: u64,
    ) -> Result<Out, Fault> {
        match action {
            "Next" | "Previous" => {
                self.require_coordinator(p)?;
                let t = &mut self.players[p].transport;
                if t.source != Source::Queue || t.track == 0 {
                    return Err(TRANSITION_NOT_AVAILABLE);
                }
                let to = if action == "Next" {
                    t.track + 1
                } else {
                    t.track - 1
                };
                if to == 0 || to > t.queue.len() {
                    return Err(ILLEGAL_SEEK_TARGET);
                }
                t.go_to_track(to, now);
                Ok(Vec::new())
            }
            "Seek" => {
                self.require_coordinator(p)?;
                let target = args.get("Target")?;
                let t = &mut self.players[p].transport;
                match args.get("Unit")? {
                    "TRACK_NR" => {
                        let n: usize = target.trim().parse().map_err(|_| INVALID_ARGS)?;
                        if t.source != Source::Queue || n == 0 || n > t.queue.len() {
                            return Err(ILLEGAL_SEEK_TARGET);
                        }
                        t.go_to_track(n, now);
                    }
                    "REL_TIME" => {
                        let ms = parse_hms(target).ok_or(INVALID_ARGS)?;
                        let d = t.duration_ms();
                        if d > 0 && ms > d {
                            return Err(ILLEGAL_SEEK_TARGET);
                        }
                        t.position_ms = ms;
                        if t.state == TransportState::Playing {
                            t.playing_since = Some(now);
                        }
                    }
                    _ => return Err(INVALID_ARGS),
                }
                Ok(Vec::new())
            }
            "BecomeCoordinatorOfStandaloneGroup" => {
                self.require_not_secondary(p)?;
                let delegated = (self.players[p].coordinator == p)
                    .then(|| self.members(p).get(1).copied())
                    .flatten();
                self.make_standalone(p);
                Ok(vec![
                    (
                        "DelegatedGroupCoordinatorID",
                        delegated
                            .map(|d| self.players[d].uuid.clone())
                            .unwrap_or_default(),
                    ),
                    ("NewGroupID", self.players[p].group_id.clone()),
                ])
            }
            _ => Err(INVALID_ACTION),
        }
    }

    /// Transport reads; a group member answers with its coordinator's state.
    fn av_reads(&self, p: usize, action: &str, now: u64) -> Result<Out, Fault> {
        match action {
            "GetTransportInfo" => {
                let c = self.players[p].coordinator;
                Ok(vec![
                    (
                        "CurrentTransportState",
                        self.players[c].transport.state.as_str().to_string(),
                    ),
                    ("CurrentTransportStatus", "OK".to_string()),
                    ("CurrentSpeed", "1".to_string()),
                ])
            }
            "GetPositionInfo" => {
                let c = self.players[p].coordinator;
                let t = &self.players[c].transport;
                let (uri, meta) = match (t.source, t.current()) {
                    (Source::Queue, Some(q)) => (q.uri.clone(), q.metadata.clone()),
                    (Source::Uri, _) => (t.uri.clone(), t.uri_metadata.clone()),
                    _ => (String::new(), String::new()),
                };
                Ok(vec![
                    ("Track", t.track.to_string()),
                    ("TrackDuration", docs::hms(t.duration_ms())),
                    ("TrackMetaData", meta),
                    ("TrackURI", uri),
                    ("RelTime", docs::hms(t.position(now))),
                    ("AbsTime", "NOT_IMPLEMENTED".to_string()),
                    ("RelCount", "2147483647".to_string()),
                    ("AbsCount", "2147483647".to_string()),
                ])
            }
            "GetMediaInfo" => {
                let c = self.players[p].coordinator;
                let t = &self.players[c].transport;
                let uri = if c == p {
                    t.uri.clone()
                } else {
                    format!("x-rincon:{}", self.players[c].uuid)
                };
                Ok(vec![
                    (
                        "NrTracks",
                        if t.source == Source::Queue {
                            t.queue.len()
                        } else {
                            usize::from(t.source == Source::Uri)
                        }
                        .to_string(),
                    ),
                    ("MediaDuration", "NOT_IMPLEMENTED".to_string()),
                    ("CurrentURI", uri),
                    ("CurrentURIMetaData", t.uri_metadata.clone()),
                    ("NextURI", String::new()),
                    ("NextURIMetaData", String::new()),
                    ("PlayMedium", "NETWORK".to_string()),
                    ("RecordMedium", "NOT_IMPLEMENTED".to_string()),
                    ("WriteStatus", "NOT_IMPLEMENTED".to_string()),
                ])
            }
            _ => Err(INVALID_ACTION),
        }
    }

    /// The URI a queue stores for an enqueued one: a bare Spotify track
    /// (`spotify%3atrack%3a…`) gets the renderer scheme and the household's
    /// account parameters, as real queues list it.
    fn queued_uri(&self, p: usize, uri: &str) -> String {
        if uri.to_ascii_lowercase().starts_with("spotify%3a") {
            let params = &self.households[self.players[p].household].params;
            format!(
                "x-sonos-spotify:{uri}?sid={}&flags={}&sn={}",
                params.sid, params.flags, params.sn
            )
        } else {
            uri.to_string()
        }
    }

    /// Reject what a real player will not render: a raw `spotify:` URI (714),
    /// or a Spotify item that does not carry this household's linked-account
    /// parameters and `desc` (800).
    fn check_renderable(&self, p: usize, uri: &str, metadata: &str) -> Result<(), Fault> {
        let lower = uri.to_ascii_lowercase();
        if lower.starts_with("spotify:") {
            return Err(ILLEGAL_MIME_TYPE);
        }
        let spotify = lower.starts_with("x-sonos-spotify:")
            || lower.starts_with("spotify%3a")
            || (lower.starts_with("x-rincon-cpcontainer:") && lower.contains("spotify%3a"));
        if !spotify {
            return Ok(());
        }
        let params = &self.households[self.players[p].household].params;
        if let Some((_, query)) = uri.split_once('?') {
            let has = |k: &str, v: u32| query.split('&').any(|kv| kv == format!("{k}={v}"));
            if !has("sid", params.sid) || !has("sn", params.sn) {
                return Err(SONOS_FAILURE);
            }
        }
        let desc_ok = parse_didl(metadata)
            .ok()
            .and_then(|objs| objs.into_iter().next())
            .and_then(|o| o.desc)
            .is_some_and(|d| d.value == params.cdudn);
        if desc_ok { Ok(()) } else { Err(SONOS_FAILURE) }
    }
}

/// The queue items an enqueued URI becomes: one track, or three for a
/// container.
fn expand(uri: &str, metadata: &str) -> Vec<QueueItem> {
    let object = parse_didl(metadata).ok().and_then(|o| o.into_iter().next());
    let title = object
        .as_ref()
        .map(|o| o.title.clone())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "Untitled".to_string());
    let track = |n: Option<usize>| QueueItem {
        uri: uri.to_string(),
        metadata: metadata.to_string(),
        title: n.map_or_else(|| title.clone(), |n| format!("{title} ({n})")),
        creator: object.as_ref().and_then(|o| o.creator.clone()),
        album: object.as_ref().and_then(|o| o.album.clone()),
        duration_ms: 180_000,
    };
    if uri.starts_with("x-rincon-cpcontainer:") {
        (1..=3).map(|n| track(Some(n))).collect()
    } else {
        vec![track(None)]
    }
}

impl State {
    fn rendering_control(&mut self, p: usize, action: &str, args: &Args<'_>) -> Result<Out, Fault> {
        let channel = |args: &Args<'_>| match args.get("Channel")? {
            "Master" => Ok(()),
            _ => Err(INVALID_ARGS),
        };
        match action {
            "GetVolume" => {
                channel(args)?;
                Ok(vec![("CurrentVolume", self.players[p].volume.to_string())])
            }
            "SetVolume" => {
                channel(args)?;
                self.players[p].volume = volume_arg(args, "DesiredVolume")?;
                Ok(Vec::new())
            }
            "SetRelativeVolume" => {
                channel(args)?;
                let adjust: i32 = args.num("Adjustment")?;
                let v = (i32::from(self.players[p].volume) + adjust).clamp(0, 100);
                self.players[p].volume = u8::try_from(v).unwrap_or(100);
                Ok(vec![("NewVolume", v.to_string())])
            }
            "RampToVolume" => {
                channel(args)?;
                args.get("RampType")?;
                let to = volume_arg(args, "DesiredVolume")?;
                let from = self.players[p].volume;
                self.players[p].volume = to;
                // Seconds a real ramp would take; the sim applies it at once.
                Ok(vec![("RampTime", u32::from(from.abs_diff(to)).to_string())])
            }
            "GetMute" => {
                channel(args)?;
                Ok(vec![(
                    "CurrentMute",
                    u8::from(self.players[p].mute).to_string(),
                )])
            }
            "SetMute" => {
                channel(args)?;
                self.players[p].mute = bool_arg(args, "DesiredMute")?;
                Ok(Vec::new())
            }
            _ => Err(INVALID_ACTION),
        }
    }

    fn group_rendering_control(
        &mut self,
        p: usize,
        action: &str,
        args: &Args<'_>,
    ) -> Result<Out, Fault> {
        self.require_coordinator(p)?;
        let members = self.members(p);
        let average = |s: &Self| {
            let total: u32 = members
                .iter()
                .map(|&m| u32::from(s.players[m].volume))
                .sum();
            let n = u32::try_from(members.len()).unwrap_or(1).max(1);
            (total + n / 2) / n
        };
        match action {
            "GetGroupVolume" => Ok(vec![("CurrentVolume", average(self).to_string())]),
            "SetGroupVolume" | "SetRelativeGroupVolume" => {
                let before = average(self);
                let desired = if action == "SetGroupVolume" {
                    u32::from(volume_arg(args, "DesiredVolume")?)
                } else {
                    let adjust: i64 = args.num("Adjustment")?;
                    u32::try_from((i64::from(before) + adjust).clamp(0, 100)).unwrap_or(0)
                };
                // Scale every member by the same ratio, as Sonos does.
                for &m in &members {
                    let v = u32::from(self.players[m].volume);
                    let scaled = (v * desired + before / 2)
                        .checked_div(before)
                        .unwrap_or(desired);
                    self.players[m].volume = u8::try_from(scaled.min(100)).unwrap_or(100);
                }
                let out = if action == "SetRelativeGroupVolume" {
                    vec![("NewVolume", average(self).to_string())]
                } else {
                    Vec::new()
                };
                Ok(out)
            }
            "GetGroupMute" => {
                let all = members.iter().all(|&m| self.players[m].mute);
                Ok(vec![("CurrentMute", u8::from(all).to_string())])
            }
            "SetGroupMute" => {
                let mute = bool_arg(args, "DesiredMute")?;
                for &m in &members {
                    self.players[m].mute = mute;
                }
                Ok(Vec::new())
            }
            "SnapshotGroupVolume" => Ok(Vec::new()),
            _ => Err(INVALID_ACTION),
        }
    }

    fn zone_group_topology(&self, p: usize, action: &str) -> Result<Out, Fault> {
        let h = self.players[p].household;
        match action {
            "GetZoneGroupState" => Ok(vec![("ZoneGroupState", docs::zone_group_state(self, h))]),
            "GetZoneGroupAttributes" => {
                let c = self.players[p].coordinator;
                let members = self.members(c);
                let names: Vec<&str> = members
                    .iter()
                    .map(|&m| self.players[m].room.as_str())
                    .collect();
                let uuids: Vec<&str> = members
                    .iter()
                    .map(|&m| self.players[m].uuid.as_str())
                    .collect();
                Ok(vec![
                    ("CurrentZoneGroupName", names.join(" + ")),
                    ("CurrentZoneGroupID", self.players[c].group_id.clone()),
                    ("CurrentZonePlayerUUIDsInGroup", uuids.join(",")),
                    ("CurrentMuseHouseholdId", self.households[h].id.clone()),
                ])
            }
            _ => Err(INVALID_ACTION),
        }
    }

    fn content_directory(&self, p: usize, action: &str, args: &Args<'_>) -> Result<Out, Fault> {
        if action != "Browse" {
            return Err(INVALID_ACTION);
        }
        let object = args.get("ObjectID")?;
        if args.get("BrowseFlag")? != "BrowseDirectChildren" {
            return Err(INVALID_ARGS);
        }
        let start: usize = args.num("StartingIndex")?;
        let count: usize = args.num("RequestedCount")?;
        let window = |total: usize| {
            let from = start.min(total);
            let to = if count == 0 {
                total
            } else {
                (from + count).min(total)
            };
            (from, to)
        };
        let (items, returned, total, update_id) = match object {
            "FV:2" => {
                let favs = &self.households[self.players[p].household].favorites;
                let (from, to) = window(favs.len());
                (
                    docs::favorite_items(&favs[from..to], from),
                    to - from,
                    favs.len(),
                    1,
                )
            }
            "Q:0" => {
                let t = &self.players[p].transport;
                let (from, to) = window(t.queue.len());
                (
                    docs::queue_items(&t.queue[from..to], from + 1),
                    to - from,
                    t.queue.len(),
                    t.queue_update_id,
                )
            }
            _ => return Err(TRANSITION_NOT_AVAILABLE),
        };
        Ok(vec![
            ("Result", docs::didl(&items)),
            ("NumberReturned", returned.to_string()),
            ("TotalMatches", total.to_string()),
            ("UpdateID", update_id.to_string()),
        ])
    }

    fn device_properties(&self, p: usize, action: &str) -> Result<Out, Fault> {
        let player = &self.players[p];
        match action {
            "GetZoneAttributes" => Ok(vec![
                ("CurrentZoneName", player.room.clone()),
                ("CurrentIcon", "x-rincon-roomicon:living".to_string()),
                ("CurrentConfiguration", "1".to_string()),
                ("CurrentTargetRoomName", player.room.clone()),
            ]),
            "GetHouseholdID" => Ok(vec![(
                "CurrentHouseholdID",
                self.households[player.household].id.clone(),
            )]),
            _ => Err(INVALID_ACTION),
        }
    }
}

fn volume_arg(args: &Args<'_>, name: &str) -> Result<u8, Fault> {
    let v: u8 = args.num(name)?;
    if v > 100 { Err(INVALID_ARGS) } else { Ok(v) }
}

fn bool_arg(args: &Args<'_>, name: &str) -> Result<bool, Fault> {
    match args.get(name)? {
        "1" | "true" => Ok(true),
        "0" | "false" => Ok(false),
        _ => Err(INVALID_ARGS),
    }
}

/// Parse `H:MM:SS` into milliseconds.
fn parse_hms(s: &str) -> Option<u64> {
    let mut parts = s.trim().split(':').map(str::parse::<u64>);
    let (h, m, sec) = (
        parts.next()?.ok()?,
        parts.next()?.ok()?,
        parts.next()?.ok()?,
    );
    (parts.next().is_none() && m < 60 && sec < 60).then_some((h * 3600 + m * 60 + sec) * 1000)
}

/// The `res duration` of the first DIDL object in `metadata`, in ms.
fn didl_duration_ms(metadata: &str) -> u64 {
    parse_didl(metadata)
        .ok()
        .and_then(|objects| objects.into_iter().next())
        .and_then(|o| o.res)
        .and_then(|r| r.duration_secs())
        .map_or(0, |secs| u64::from(secs) * 1000)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::docs::{
        AV_TRANSPORT, CONTENT_DIRECTORY, GROUP_RENDERING_CONTROL, RENDERING_CONTROL,
    };

    /// S1: Kitchen (0), Office (1), Bridge (2). S2: Living Room (3).
    fn state() -> State {
        let mut s = State::new(
            vec![crate::household(1), crate::household(2)],
            SimClock::default(),
        );
        s.add_player("Kitchen", SimModel::Play5Gen1, 0);
        s.add_player("Office", SimModel::Play5Gen1, 0);
        s.add_player("Bridge", SimModel::Bridge, 0);
        s.add_player("Living Room", SimModel::One, 1);
        s
    }

    fn call(
        s: &mut State,
        p: usize,
        svc: &ServiceDef,
        action: &str,
        args: &[(&str, &str)],
    ) -> Result<Out, Fault> {
        let args: Vec<(String, String)> = args
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        s.invoke(p, svc, action, &Args(&args))
    }

    fn get(out: &Out, name: &str) -> String {
        out.iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.clone())
            .unwrap()
    }

    /// The bare enqueue URI and DIDL a household's track favorites replay.
    fn spotify_track(s: &State, h: usize, id: &str, desc: &str) -> (String, String) {
        let prefix = &s.households[h].params.item_id_prefix;
        let enc = format!("spotify%3atrack%3a{id}");
        let meta = docs::item_metadata(
            &format!("{prefix}{enc}"),
            &format!("Track {id}"),
            "object.item.audioItem.musicTrack",
            desc,
        );
        (enc, meta)
    }

    fn enqueue(s: &mut State, p: usize, uri: &str, meta: &str) -> Result<Out, Fault> {
        call(
            s,
            p,
            &AV_TRANSPORT,
            "AddURIToQueue",
            &[
                ("InstanceID", "0"),
                ("EnqueuedURI", uri),
                ("EnqueuedURIMetaData", meta),
                ("DesiredFirstTrackNumberEnqueued", "0"),
                ("EnqueueAsNext", "0"),
            ],
        )
    }

    fn transport_state(s: &mut State, p: usize) -> String {
        get(
            &call(
                s,
                p,
                &AV_TRANSPORT,
                "GetTransportInfo",
                &[("InstanceID", "0")],
            )
            .unwrap(),
            "CurrentTransportState",
        )
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one ordered scenario; splitting it loses the sequence
    fn queue_flow_from_the_protocol_notes() {
        let mut s = state();
        let cdudn = s.households[0].params.cdudn.clone();
        for (n, id) in ["T1", "T2"].iter().enumerate() {
            let (uri, meta) = spotify_track(&s, 0, id, &cdudn);
            let out = enqueue(&mut s, 0, &uri, &meta).unwrap();
            assert_eq!(get(&out, "FirstTrackNumberEnqueued"), (n + 1).to_string());
            assert_eq!(get(&out, "NewQueueLength"), (n + 1).to_string());
        }
        // Nothing to play yet: the source is not set.
        assert_eq!(
            call(
                &mut s,
                0,
                &AV_TRANSPORT,
                "Play",
                &[("InstanceID", "0"), ("Speed", "1")]
            ),
            Err(TRANSITION_NOT_AVAILABLE)
        );
        let queue = format!("x-rincon-queue:{}#0", s.players[0].uuid);
        call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "SetAVTransportURI",
            &[
                ("InstanceID", "0"),
                ("CurrentURI", &queue),
                ("CurrentURIMetaData", ""),
            ],
        )
        .unwrap();
        call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "Seek",
            &[("InstanceID", "0"), ("Unit", "TRACK_NR"), ("Target", "2")],
        )
        .unwrap();
        call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "Play",
            &[("InstanceID", "0"), ("Speed", "1")],
        )
        .unwrap();
        assert_eq!(transport_state(&mut s, 0), "PLAYING");

        s.clock.advance(std::time::Duration::from_secs(30));
        let pos = call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "GetPositionInfo",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(
            (
                get(&pos, "Track"),
                get(&pos, "RelTime"),
                get(&pos, "TrackDuration")
            ),
            ("2".into(), "0:00:30".into(), "0:03:00".into())
        );
        assert!(
            get(&pos, "TrackURI")
                .starts_with("x-sonos-spotify:spotify%3atrack%3aT2?sid=12&flags=8224&sn=1")
        );

        assert_eq!(
            call(&mut s, 0, &AV_TRANSPORT, "Next", &[("InstanceID", "0")]),
            Err(ILLEGAL_SEEK_TARGET)
        );
        call(&mut s, 0, &AV_TRANSPORT, "Previous", &[("InstanceID", "0")]).unwrap();
        call(&mut s, 0, &AV_TRANSPORT, "Pause", &[("InstanceID", "0")]).unwrap();
        assert_eq!(transport_state(&mut s, 0), "PAUSED_PLAYBACK");
        assert_eq!(
            call(&mut s, 0, &AV_TRANSPORT, "Pause", &[("InstanceID", "0")]),
            Err(TRANSITION_NOT_AVAILABLE)
        );
        // Paused positions do not move.
        s.clock.advance(std::time::Duration::from_secs(10));
        let pos = call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "GetPositionInfo",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(
            (get(&pos, "Track"), get(&pos, "RelTime")),
            ("1".into(), "0:00:00".into())
        );

        call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "RemoveTrackFromQueue",
            &[
                ("InstanceID", "0"),
                ("ObjectID", "Q:0/2"),
                ("UpdateID", "0"),
            ],
        )
        .unwrap();
        assert_eq!(s.players[0].transport.queue.len(), 1);
        call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "RemoveAllTracksFromQueue",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(transport_state(&mut s, 0), "STOPPED");
    }

    #[test]
    fn spotify_items_need_the_households_own_params() {
        let mut s = state();
        let cdudn = s.households[0].params.cdudn.clone();
        // A raw spotify: URI is the wrong scheme.
        let (_, meta) = spotify_track(&s, 0, "X", &cdudn);
        assert_eq!(
            enqueue(&mut s, 0, "spotify:track:X", &meta),
            Err(ILLEGAL_MIME_TYPE)
        );
        // Wrong account descriptor.
        let (uri, bad) = spotify_track(&s, 0, "X", "SA_RINCON2311_X_#Svc2311-0-Token");
        assert_eq!(enqueue(&mut s, 0, &uri, &bad), Err(SONOS_FAILURE));
        // S1's sn on the S2 household.
        let s1 = s.households[0].params.clone();
        let direct = format!(
            "x-sonos-spotify:spotify%3atrack%3aX?sid={}&flags={}&sn={}",
            s1.sid, s1.flags, s1.sn
        );
        let set = |s: &mut State, p, uri: &str, meta: &str| {
            call(
                s,
                p,
                &AV_TRANSPORT,
                "SetAVTransportURI",
                &[
                    ("InstanceID", "0"),
                    ("CurrentURI", uri),
                    ("CurrentURIMetaData", meta),
                ],
            )
        };
        assert_eq!(set(&mut s, 3, &direct, &meta), Err(SONOS_FAILURE));
        set(&mut s, 0, &direct, &meta).unwrap();
        // Every favorite a household lists renders on that household.
        for (h, p) in [(0, 0), (1, 3)] {
            for f in s.households[h].favorites.clone() {
                set(&mut s, p, &f.uri, &f.metadata)
                    .unwrap_or_else(|e| panic!("{}: {e:?}", f.title));
            }
        }
        // A non-Spotify stream needs no params.
        set(
            &mut s,
            0,
            "x-rincon-mp3radio://stream.example.invalid/x.mp3",
            "",
        )
        .unwrap();
    }

    #[test]
    fn grouping_moves_coordination() {
        let mut s = state();
        let kitchen = format!("x-rincon:{}", s.players[0].uuid);
        let join = |s: &mut State, p, uri: &str| {
            call(
                s,
                p,
                &AV_TRANSPORT,
                "SetAVTransportURI",
                &[
                    ("InstanceID", "0"),
                    ("CurrentURI", uri),
                    ("CurrentURIMetaData", ""),
                ],
            )
        };
        join(&mut s, 1, &kitchen).unwrap();
        assert_eq!(s.members(0), [0, 1]);
        assert_eq!(s.players[1].group_id, s.players[0].group_id);
        assert_eq!(s.coordinators(0), [0, 2]);
        // Members refuse coordinator verbs; reads follow the coordinator.
        assert_eq!(
            call(
                &mut s,
                1,
                &AV_TRANSPORT,
                "Play",
                &[("InstanceID", "0"), ("Speed", "1")]
            ),
            Err(SONOS_FAILURE)
        );
        let media = call(
            &mut s,
            1,
            &AV_TRANSPORT,
            "GetMediaInfo",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(get(&media, "CurrentURI"), kitchen);
        // Cross-household joins and joins to a bridge fail.
        let living = format!("x-rincon:{}", s.players[3].uuid);
        assert_eq!(join(&mut s, 0, &living), Err(SONOS_FAILURE));
        let bridge = format!("x-rincon:{}", s.players[2].uuid);
        assert_eq!(join(&mut s, 0, &bridge), Err(SONOS_FAILURE));
        // The coordinator leaving hands the group to its member.
        let out = call(
            &mut s,
            0,
            &AV_TRANSPORT,
            "BecomeCoordinatorOfStandaloneGroup",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(get(&out, "DelegatedGroupCoordinatorID"), s.players[1].uuid);
        assert_eq!((s.players[0].coordinator, s.players[1].coordinator), (0, 1));
        assert_ne!(s.players[0].group_id, s.players[1].group_id);
        let zgs = docs::zone_group_state(&s, 0);
        assert_eq!(zgs.matches("<ZoneGroup ").count(), 3);
    }

    #[test]
    fn stereo_pairs_move_as_one() {
        let mut s = State::new(vec![crate::household(1)], SimClock::default());
        let den = s.add_pair("Den", SimModel::Play5Gen1, 0);
        let kitchen = s.add_player("Kitchen", SimModel::Play5Gen1, 0);
        let rf = s.secondary_of(den).unwrap();
        assert_eq!(s.members(den), [den, rf]);
        assert_ne!(s.players[den].ip, s.players[rf].ip);
        let zgs = docs::zone_group_state(&s, 0);
        assert_eq!(zgs.matches("ChannelMapSet=").count(), 2);
        assert_eq!(zgs.matches("Invisible=\"1\"").count(), 1);
        assert!(zgs.contains("Location=\"http://192.0.2.10:1400/xml/device_description.xml\""));

        let join = |s: &mut State, p, target: usize| {
            let uri = format!("x-rincon:{}", s.players[target].uuid);
            call(
                s,
                p,
                &AV_TRANSPORT,
                "SetAVTransportURI",
                &[
                    ("InstanceID", "0"),
                    ("CurrentURI", &uri),
                    ("CurrentURIMetaData", ""),
                ],
            )
        };
        // The hidden half refuses group verbs; the primary carries both.
        assert_eq!(join(&mut s, rf, kitchen), Err(SONOS_FAILURE));
        assert_eq!(
            call(
                &mut s,
                rf,
                &AV_TRANSPORT,
                "BecomeCoordinatorOfStandaloneGroup",
                &[("InstanceID", "0")]
            ),
            Err(SONOS_FAILURE)
        );
        join(&mut s, den, kitchen).unwrap();
        assert_eq!(s.members(kitchen), [kitchen, den, rf]);
        assert_eq!(s.players[rf].group_id, s.players[kitchen].group_id);
        // Kitchen leaving hands the group to the pair's primary, never its hidden half.
        call(
            &mut s,
            kitchen,
            &AV_TRANSPORT,
            "BecomeCoordinatorOfStandaloneGroup",
            &[("InstanceID", "0")],
        )
        .unwrap();
        assert_eq!(s.members(den), [den, rf]);
        assert_eq!(s.members(kitchen), [kitchen]);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one ordered scenario; splitting it loses the sequence
    fn volumes_and_group_volume_scaling() {
        let mut s = state();
        let set = |s: &mut State, p, v: &str| {
            call(
                s,
                p,
                &RENDERING_CONTROL,
                "SetVolume",
                &[
                    ("InstanceID", "0"),
                    ("Channel", "Master"),
                    ("DesiredVolume", v),
                ],
            )
        };
        assert_eq!(set(&mut s, 0, "101"), Err(INVALID_ARGS));
        assert_eq!(
            call(
                &mut s,
                0,
                &RENDERING_CONTROL,
                "SetVolume",
                &[
                    ("InstanceID", "0"),
                    ("Channel", "LF"),
                    ("DesiredVolume", "5")
                ]
            ),
            Err(INVALID_ARGS)
        );
        set(&mut s, 1, "40").unwrap();
        let kitchen = format!("x-rincon:{}", s.players[0].uuid);
        call(
            &mut s,
            1,
            &AV_TRANSPORT,
            "SetAVTransportURI",
            &[
                ("InstanceID", "0"),
                ("CurrentURI", &kitchen),
                ("CurrentURIMetaData", ""),
            ],
        )
        .unwrap();
        let group = |s: &mut State, action, args: &[(&str, &str)]| {
            call(s, 0, &GROUP_RENDERING_CONTROL, action, args)
        };
        assert_eq!(
            get(
                &group(&mut s, "GetGroupVolume", &[("InstanceID", "0")]).unwrap(),
                "CurrentVolume"
            ),
            "30"
        );
        group(
            &mut s,
            "SetGroupVolume",
            &[("InstanceID", "0"), ("DesiredVolume", "60")],
        )
        .unwrap();
        assert_eq!((s.players[0].volume, s.players[1].volume), (40, 80));
        let out = group(
            &mut s,
            "SetRelativeGroupVolume",
            &[("InstanceID", "0"), ("Adjustment", "-30")],
        )
        .unwrap();
        assert_eq!(get(&out, "NewVolume"), "30");
        assert_eq!((s.players[0].volume, s.players[1].volume), (20, 40));
        group(
            &mut s,
            "SetGroupMute",
            &[("InstanceID", "0"), ("DesiredMute", "1")],
        )
        .unwrap();
        assert!(s.players[0].mute && s.players[1].mute);
        // Group volume is a coordinator verb.
        assert_eq!(
            call(
                &mut s,
                1,
                &GROUP_RENDERING_CONTROL,
                "GetGroupVolume",
                &[("InstanceID", "0")]
            ),
            Err(SONOS_FAILURE)
        );
        let out = call(
            &mut s,
            0,
            &RENDERING_CONTROL,
            "RampToVolume",
            &[
                ("InstanceID", "0"),
                ("Channel", "Master"),
                ("RampType", "SLEEP_TIMER_RAMP_TYPE"),
                ("DesiredVolume", "10"),
                ("ResetVolumeAfter", "0"),
                ("ProgramURI", ""),
            ],
        )
        .unwrap();
        assert_eq!(
            (get(&out, "RampTime"), s.players[0].volume),
            ("10".into(), 10)
        );
    }

    #[test]
    fn browse_pages_and_faults() {
        let mut s = state();
        let browse = |s: &mut State, object: &str, start: &str, count: &str| {
            call(
                s,
                0,
                &CONTENT_DIRECTORY,
                "Browse",
                &[
                    ("ObjectID", object),
                    ("BrowseFlag", "BrowseDirectChildren"),
                    ("Filter", "*"),
                    ("StartingIndex", start),
                    ("RequestedCount", count),
                    ("SortCriteria", ""),
                ],
            )
        };
        let all = browse(&mut s, "FV:2", "0", "0").unwrap();
        assert_eq!(
            (get(&all, "NumberReturned"), get(&all, "TotalMatches")),
            ("5".into(), "5".into())
        );
        let page = browse(&mut s, "FV:2", "3", "10").unwrap();
        assert_eq!(get(&page, "NumberReturned"), "2");
        assert!(get(&page, "Result").contains("id=\"FV:2/3\""));
        assert_eq!(
            browse(&mut s, "NOPE:0", "0", "5"),
            Err(TRANSITION_NOT_AVAILABLE)
        );
        let empty = browse(&mut s, "Q:0", "0", "5").unwrap();
        assert_eq!(get(&empty, "TotalMatches"), "0");
        assert_eq!(
            call(&mut s, 0, &CONTENT_DIRECTORY, "Search", &[]),
            Err(INVALID_ACTION)
        );
        assert_eq!(
            call(
                &mut s,
                0,
                &CONTENT_DIRECTORY,
                "Browse",
                &[("ObjectID", "FV:2")]
            ),
            Err(INVALID_ARGS)
        );
    }
}
