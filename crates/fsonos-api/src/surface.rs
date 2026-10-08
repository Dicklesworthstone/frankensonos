//! The speakers a surface acts on, shared by the HTTP API and the MCP tools.
//!
//! A [`Surface`] holds the LAN transport, how to find the households (a
//! survey, cached for a short while), the house policy, and the clock. Each
//! call names its [`Client`], so one surface serves callers with different
//! identities (local agents, tailnet principals, unknown peers).
//!
//! With an action log ([`Surface::with_action_log`]), [`Surface::control`] is
//! the single choke point every mutating call goes through: authorize, plan,
//! snapshot the zones it touches, carry it out under the policy, and log who
//! asked, the policy's decision (allow / clamp / deny) and what happened.
//! [`Surface::undo`] puts the newest undoable action back.
//!
//! With the daemon's live model ([`Surface::with_live`]) the households and
//! the zones' playback come from GENA events instead of surveys and polls;
//! for a moment after a regroup, reads survey directly so a caller sees its
//! own change before the events arrive.

use fsonos_core::actions::{self, UndoReport};
use fsonos_core::clock::Clock;
use fsonos_core::doctor::{self, Report, Runner};
use fsonos_core::live::{Live, LiveEvent};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{Action, ActionFilter, LoggedAction, Store, StoreError};
use fsonos_core::{HouseholdState, control};
use fsonos_core::{favorites, search};
use fsonos_proto::Transport;
use fsonos_types::{PlayerId, TransportState};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::dj::{DjEngine, DjSpeakers};
use crate::events::EventBus;
use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::Guard;
use crate::plan::{Command, plan_play_favorite, resolve};
use crate::reads::{FavoriteDto, HitDto, PlayDto, TrackDto, ZoneStateDto};
use crate::request::{PlayFavoriteRequest, SearchRequest};
use crate::zones::{ZoneDto, zone_for_target, zone_views};

/// Finds the households (a LAN survey, say).
pub type Survey = Box<dyn Fn(&dyn Transport) -> Result<Vec<HouseholdState>, Failure> + Send + Sync>;

/// How long a survey answer is reused before the next call surveys again.
pub const REFRESH: Duration = Duration::from_secs(30);

/// How long after a regroup reads survey directly rather than trust the live
/// model, whose topology events may not have arrived yet.
pub const SETTLE: Duration = Duration::from_secs(3);

/// See the module docs.
pub struct Surface {
    transport: Arc<dyn Transport + Send + Sync>,
    survey: Survey,
    cache: Mutex<Option<(Instant, Vec<HouseholdState>)>>,
    policy: Policy,
    clock: Box<dyn Clock>,
    log: Option<ActionLog>,
    doctor_checks: Option<DoctorChecks>,
    /// Runs the DJ (`dj_start` / `dj_skip` / `dj_stop`); without one they
    /// answer NOT_IMPLEMENTED.
    dj: Option<Box<dyn DjEngine>>,
    /// The daemon's live model; not kept alive by the surface (see
    /// [`Surface::with_live`]).
    live: Option<Weak<Live>>,
    /// The event stream, fed by the live model and the action log.
    events: Option<Arc<EventBus>>,
    /// Until when reads survey directly (set by a regroup).
    settle_until: Mutex<Option<Instant>>,
}

/// Doctor checks a surface adds to the core's (the daemon's bind and health
/// checks, say); registered on each run.
pub type DoctorChecks = Box<dyn Fn(&mut Runner) + Send + Sync>;

/// Where a surface logs its actions, and its name in the log.
struct ActionLog {
    store: Mutex<Box<dyn Store + Send>>,
    surface: String,
}

impl Surface {
    #[must_use]
    pub fn new(
        transport: Box<dyn Transport + Send + Sync>,
        survey: Survey,
        policy: Policy,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            transport: Arc::from(transport),
            survey,
            cache: Mutex::new(None),
            policy,
            clock,
            log: None,
            doctor_checks: None,
            dj: None,
            live: None,
            events: None,
            settle_until: Mutex::new(None),
        }
    }

    /// Read the households and playback from `live` (the daemon's model,
    /// kept current by GENA) instead of surveying and polling. The surface
    /// does not keep the model alive: its owner stops it (every subscription
    /// ends) by dropping the last `Arc`, after which the surface surveys and
    /// polls again.
    ///
    /// It also starts the event stream ([`Self::events`]): the model's
    /// changes and every logged action, published to an [`EventBus`].
    #[must_use]
    pub fn with_live(mut self, live: &Arc<Live>) -> Self {
        self.live = Some(Arc::downgrade(live));
        let bus = Arc::new(EventBus::new());
        crate::events::feed(&bus, live);
        self.events = Some(bus);
        self
    }

    /// The event stream (`events`, read-only): only a surface over the
    /// daemon's live model has one.
    pub fn events(&self, client: &Client) -> Result<Arc<EventBus>, Failure> {
        self.guard(client).authorize("events", true)?;
        self.events.clone().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotImplemented,
                "the event stream needs the daemon's live model",
            )
            .with_hint("Connect to `fsonos serve`, which keeps one.")
        })
    }

    fn live(&self) -> Option<Arc<Live>> {
        self.live.as_ref().and_then(Weak::upgrade)
    }

    /// Run `dj_start` / `dj_skip` / `dj_stop` with `engine`; with a live
    /// model, [`follow`] keeps its queues fed.
    #[must_use]
    pub fn with_dj(mut self, engine: Box<dyn DjEngine>) -> Self {
        self.dj = Some(engine);
        self
    }

    /// Fold `player`'s latest playback into the DJ's feed, if it feeds that
    /// coordinator (see [`follow`]).
    fn dj_playback(&self, player: &PlayerId) {
        let (Some(dj), Some(live)) = (&self.dj, self.live()) else {
            return;
        };
        if !dj.feeds(player) {
            return;
        }
        let Some(playback) = live.player(player) else {
            return;
        };
        let households = live.households();
        let at = DjSpeakers {
            transport: &*self.transport,
            households: &households,
            coordinator: player,
        };
        let now = self.now();
        let fed = self.with_store(|store| {
            dj.on_playback(at, store, &playback, now);
            Ok(())
        });
        if let Ok(None) = fed {
            tracing::warn!("the DJ needs the daemon's store to record and top up");
        }
    }

    /// Add `checks` to every doctor run.
    #[must_use]
    pub fn with_doctor_checks(mut self, checks: DoctorChecks) -> Self {
        self.doctor_checks = Some(checks);
        self
    }

    /// Run the doctor (`doctor`, read-only): the surface's own checks, then
    /// the core's Spotify linkage checks for each household. It runs even
    /// when nothing answers, which is what it is for.
    pub fn doctor(&self, client: &Client) -> Result<Report, Failure> {
        self.guard(client).authorize("doctor", true)?;
        let households = self.households().unwrap_or_default();
        let mut runner = Runner::new();
        if let Some(checks) = &self.doctor_checks {
            checks(&mut runner);
        }
        if let Some(live) = self.live() {
            runner.register(crate::live::LiveCheck {
                snapshot: live.snapshot(),
            });
        }
        doctor::spotify::register(&mut runner, &self.transport, &households);
        runner
            .run()
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("doctor: {e}")))
    }

    /// Log every mutating call in `store` under `surface` (`http`, `mcp`,
    /// `cli`, ...), which also enables [`Self::undo`].
    #[must_use]
    pub fn with_action_log(mut self, store: Box<dyn Store + Send>, surface: &str) -> Self {
        self.log = Some(ActionLog {
            store: Mutex::new(store),
            surface: surface.to_string(),
        });
        self
    }

    fn now(&self) -> i64 {
        self.clock.now().timestamp()
    }

    /// Append to the action log, if there is one. A log that cannot be
    /// written never fails the call it records.
    fn record(
        &self,
        client: &Client,
        intent: String,
        decision: String,
        result: String,
        before_state: Option<String>,
    ) {
        let Some(log) = &self.log else {
            return;
        };
        let action = Action {
            at: self.now(),
            client: client.key().to_string(),
            surface: log.surface.clone(),
            intent,
            decision,
            result,
            before_state,
            undo_of: None,
        };
        let written = match log.store.lock() {
            Ok(mut store) => actions::record(&mut **store, &action).map_err(|e| e.to_string()),
            Err(_) => Err("action log poisoned".to_string()),
        };
        match written {
            Ok(id) => {
                if let Some(bus) = &self.events {
                    let logged = LoggedAction { id, action };
                    if let Ok(data) = serde_json::to_value(crate::log::ActionDto::from(&logged)) {
                        bus.publish("action.logged", data);
                    }
                }
            }
            Err(e) => tracing::warn!("action not logged ({}): {e}", action.intent),
        }
    }

    fn guard<'a>(&'a self, client: &'a Client) -> Guard<'a> {
        Guard {
            policy: &self.policy,
            client,
            clock: &*self.clock,
        }
    }

    /// The households: the live model's, or the last survey's while it is
    /// fresh and found rooms (always a survey for a moment after a regroup).
    pub fn households(&self) -> Result<Vec<HouseholdState>, Failure> {
        if let Some(live) = self.live()
            && !self.settling()
        {
            return crate::live::households(&live);
        }
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "household cache poisoned"))?;
        if let Some((at, households)) = cache.as_ref()
            && at.elapsed() < REFRESH
            && households.iter().any(|h| !h.rooms.is_empty())
        {
            return Ok(households.clone());
        }
        let households = (self.survey)(&*self.transport)?;
        *cache = Some((Instant::now(), households.clone()));
        Ok(households)
    }

    /// Forget the cached survey, so the next call sees the speakers as they
    /// are now (after a regroup, or before an undo plans its restore). The
    /// live model surveys soon; until it settles, reads survey directly.
    fn invalidate(&self) {
        if let Ok(mut cache) = self.cache.lock() {
            *cache = None;
        }
        if let Some(live) = self.live() {
            live.refresh_soon();
            if let Ok(mut until) = self.settle_until.lock() {
                *until = Some(Instant::now() + SETTLE);
            }
        }
    }

    fn settling(&self) -> bool {
        self.settle_until
            .lock()
            .ok()
            .and_then(|until| *until)
            .is_some_and(|until| Instant::now() < until)
    }

    /// `failure`, or `PLAYER_UNREACHABLE` when it names a room the live
    /// model lists as vanished (powered off), not one that does not exist.
    fn explain(&self, failure: Failure) -> Failure {
        match self.live() {
            Some(live) => crate::live::vanished(failure, &live.snapshot()),
            None => failure,
        }
    }

    /// Ask the live model to survey soon when `failure` says a player is
    /// gone (moved, rebooted, or off).
    fn notice(&self, failure: &Failure) {
        if failure.code != ErrorCode::PlayerUnreachable {
            return;
        }
        match self.live() {
            Some(live) => live.refresh_soon(),
            // The next call surveys again, so the caller's retry finds it.
            None => {
                if let Ok(mut cache) = self.cache.lock() {
                    *cache = None;
                }
            }
        }
    }

    /// Carry `command` out, retrying once where the speakers changed under
    /// it ([`crate::heal`]).
    fn execute(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        if let Command::Dj {
            coordinator,
            action,
        } = command
            && let Some(dj) = &self.dj
        {
            let at = DjSpeakers {
                transport: &*self.transport,
                households,
                coordinator,
            };
            let now = self.now();
            return self
                .with_store(|store| Ok(dj.act(at, store, *action, now)))?
                .unwrap_or_else(|| {
                    Err(Failure::new(
                        ErrorCode::Internal,
                        "the DJ needs the daemon's store (its library and history)",
                    ))
                });
        }
        crate::heal::execute_healing(&*self.transport, households, guard, command, || {
            self.resurvey()
        })
    }

    /// The speakers as a survey finds them now: the live model's next
    /// survey (up to 5 s), or a direct one, which the cache keeps.
    fn resurvey(&self) -> Option<Vec<HouseholdState>> {
        if let Some(live) = self.live() {
            return crate::live::resurvey(&live, Duration::from_secs(5));
        }
        let households = (self.survey)(&*self.transport).ok()?;
        if let Ok(mut cache) = self.cache.lock() {
            *cache = Some((Instant::now(), households.clone()));
        }
        Some(households)
    }

    /// A control call: authorize `tool` for `client`, plan against the
    /// households, and carry it out under the policy (logged when the
    /// surface keeps an action log).
    pub fn control(
        &self,
        client: &Client,
        tool: &str,
        plan: impl FnOnce(&[HouseholdState]) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        self.control_io(client, tool, |_, households| plan(households))
    }

    /// [`Self::control`] for plans that read from the speakers first (a
    /// household's favorites, say); the plan gets the transport too.
    pub fn control_io(
        &self,
        client: &Client,
        tool: &str,
        plan: impl FnOnce(&dyn Transport, &[HouseholdState]) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize(tool, false) {
            self.record(
                client,
                tool.to_string(),
                format!("deny: {}", denied.detail),
                denied.detail.clone(),
                None,
            );
            return Err(denied);
        }
        let households = self.households()?;
        let command = plan(&*self.transport, &households).map_err(|f| self.explain(f))?;
        let regroups = matches!(command, Command::Join { .. } | Command::Leave { .. });
        // Already satisfied requests change nothing and are not logged.
        if self.log.is_none() || matches!(command, Command::Nothing { .. }) {
            let result = self.execute(&households, &guard, &command);
            if result.is_ok() {
                self.remember_play(&command);
            }
            if regroups {
                self.invalidate();
            }
            if let Err(f) = &result {
                self.notice(f);
            }
            return result;
        }
        let intent = format!("{tool}: {command:?}");
        let (snaps, missed) = actions::capture_zones(
            &*self.transport,
            &households,
            &affected(&households, &command),
            self.now(),
        );
        let result = self.execute(&households, &guard, &command);
        if result.is_ok() {
            self.remember_play(&command);
        }
        if regroups {
            self.invalidate();
        }
        if let Err(f) = &result {
            self.notice(f);
        }
        let (decision, mut text, before) = match &result {
            Ok(outcome) => {
                let noted = |code: NoteCode| {
                    outcome
                        .notes
                        .iter()
                        .filter(|n| n.code == code)
                        .map(|n| n.detail.as_str())
                        .collect::<Vec<_>>()
                };
                let clamps = noted(NoteCode::VolumeClamped);
                let decision = if clamps.is_empty() {
                    "allow".to_string()
                } else {
                    format!("clamp: {}", clamps.join("; "))
                };
                let mut text = outcome.done.clone();
                for healed in noted(NoteCode::Healed) {
                    let _ = write!(text, " (healed: {healed})");
                }
                (decision, text, actions::before_state(&snaps))
            }
            Err(f) if f.code == ErrorCode::PolicyDenied => {
                (format!("deny: {}", f.detail), f.detail.clone(), None)
            }
            // It may have half happened: keep the before-state for undo.
            Err(f) => (
                "allow".to_string(),
                format!("failed: {}", f.detail),
                actions::before_state(&snaps),
            ),
        };
        if !missed.is_empty() {
            let _ = write!(
                text,
                " (before-state missing for {} zone(s): undo cannot fully restore)",
                missed.len()
            );
        }
        self.record(client, intent, decision, text, before);
        result
    }

    /// Undo the newest undoable action — only `client`'s own when
    /// `own_only` — and log the undo. `Ok(None)` when there is nothing to
    /// undo.
    pub fn undo(&self, client: &Client, own_only: bool) -> Result<Option<UndoReport>, Failure> {
        let Some(log) = &self.log else {
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                "undo needs the action log, which this surface does not keep",
            ));
        };
        self.guard(client).authorize("undo", false)?;
        // Plan the restore against the speakers as they are now.
        self.invalidate();
        let households = self.households()?;
        let mut store = log
            .store
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "action log poisoned"))?;
        let report = actions::undo_last(
            &*self.transport,
            &households,
            &mut **store,
            own_only.then_some(client),
            client,
            &log.surface,
            self.now(),
        );
        self.invalidate();
        Ok(report?)
    }

    /// The action log, newest first (empty without one).
    pub fn recent_actions(
        &self,
        client: &Client,
        filter: &ActionFilter,
    ) -> Result<Vec<LoggedAction>, Failure> {
        self.guard(client).authorize("recent_actions", true)?;
        let Some(log) = &self.log else {
            return Ok(Vec::new());
        };
        let store = log
            .store
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "action log poisoned"))?;
        store
            .recent_actions(filter)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
    }

    /// Every zone with its live transport state (`unknown` when a
    /// coordinator does not answer).
    pub fn zones(&self, client: &Client) -> Result<Vec<ZoneDto>, Failure> {
        self.guard(client).authorize("list_zones", true)?;
        let households = self.households()?;
        if households.iter().all(|h| h.rooms.is_empty()) {
            return Err(Failure::new(ErrorCode::NotReady, "no Sonos rooms answered"));
        }
        Ok(zone_views(&households, |c| {
            self.transport_state(&households, c)
        }))
    }

    /// The zone `room` plays in.
    pub fn zone(&self, client: &Client, room: &str) -> Result<ZoneDto, Failure> {
        self.guard(client).authorize("get_zone", true)?;
        let households = self.households()?;
        let target = resolve(&households, room).map_err(|f| self.explain(f))?;
        Ok(zone_for_target(&households, &target, |c| {
            self.transport_state(&households, c)
        }))
    }

    /// Play a Sonos favorite of the zone's household (`play_favorite`).
    pub fn play_favorite(
        &self,
        client: &Client,
        req: &PlayFavoriteRequest,
    ) -> Result<OutcomeDto, Failure> {
        self.control_io(client, "play_favorite", |transport, households| {
            let target = resolve(households, req.zone()?)?;
            let household_favorites =
                favorites::list(transport, households, &target.coordinator.id)?;
            plan_play_favorite(households, req, &household_favorites)
        })
    }

    /// Search the owner's saved library and, with a zone, its household's
    /// Sonos favorites (`search_library`, read-only). Without a store there
    /// is no library, only favorites.
    pub fn search_library(
        &self,
        client: &Client,
        req: &SearchRequest,
    ) -> Result<Vec<HitDto>, Failure> {
        self.guard(client).authorize("search_library", true)?;
        let (query, limit) = (req.query()?, req.limit()?);
        let library = self.with_store(|s| s.library())?.unwrap_or_default();
        let household_favorites = match req.zone()? {
            Some(zone) => {
                let households = self.households()?;
                let target = resolve(&households, zone).map_err(|f| self.explain(f))?;
                favorites::list(&*self.transport, &households, &target.coordinator.id)?
            }
            None => Vec::new(),
        };
        Ok(search::search(&library, &household_favorites, query, limit)
            .iter()
            .map(HitDto::from)
            .collect())
    }

    /// What played, newest first (`recent_plays`, read-only): in `zone`'s
    /// group (as recorded under its current coordinator), or everywhere.
    pub fn recent_plays(
        &self,
        client: &Client,
        zone: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlayDto>, Failure> {
        self.guard(client).authorize("recent_plays", true)?;
        let households = match zone {
            Some(_) => self.households()?,
            // Only to name the rooms: a history read never needs the speakers.
            None => self.households().unwrap_or_default(),
        };
        let key = zone
            .map(|z| resolve(&households, z).map(|t| t.coordinator.id.0.clone()))
            .transpose()
            .map_err(|f| self.explain(f))?;
        let plays = self
            .with_store(|s| s.recent_plays(key.as_deref(), limit))?
            .unwrap_or_default();
        Ok(plays
            .iter()
            .rev()
            .map(|p| PlayDto {
                room: control::locate(&households, &PlayerId(p.zone.clone()))
                    .ok()
                    .map(|player| player.room_name.clone()),
                source_uri: p.source_uri.clone(),
                played_at: p.played_at,
            })
            .collect())
    }

    /// Run `f` on the store, if the surface keeps one (with its action log).
    fn with_store<R>(
        &self,
        f: impl FnOnce(&mut dyn Store) -> Result<R, StoreError>,
    ) -> Result<Option<R>, Failure> {
        let Some(log) = &self.log else {
            return Ok(None);
        };
        let mut store = log
            .store
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "store poisoned"))?;
        f(&mut **store)
            .map(Some)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
    }

    /// Add a play that went through to the history, under the group's
    /// coordinator (as the DJ records its own), so the DJ hears about it.
    fn remember_play(&self, command: &Command) {
        let (coordinator, uri) = match command {
            Command::Play {
                coordinator,
                source_uri,
                ..
            } => (coordinator, Some(source_uri.as_str())),
            Command::PlayFavorite {
                coordinator,
                favorite,
            } => (coordinator, favorite.uri.as_deref()),
            _ => return,
        };
        let Some(uri) = uri else { return };
        let at = self.now();
        if let Err(e) = self.with_store(|s| s.record_play(&coordinator.0, uri, at)) {
            tracing::warn!("play not recorded ({uri}): {}", e.detail);
        }
    }

    /// The favorites of the household `zone` belongs to (`list_favorites`).
    pub fn favorites(&self, client: &Client, zone: &str) -> Result<Vec<FavoriteDto>, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        let households = self.households()?;
        let target = resolve(&households, zone).map_err(|f| self.explain(f))?;
        let listed = favorites::list(&*self.transport, &households, &target.coordinator.id)?;
        Ok(listed.iter().map(FavoriteDto::from).collect())
    }

    /// What `zone` is doing right now (`get_zone_state`): its group, the
    /// group's transport and track, and the room's own volume.
    pub fn zone_state(&self, client: &Client, zone: &str) -> Result<ZoneStateDto, Failure> {
        self.guard(client).authorize("get_zone_state", true)?;
        let households = self.households()?;
        let target = resolve(&households, zone).map_err(|f| self.explain(f))?;
        let heard = |p: &PlayerId| self.live().and_then(|live| live.player(p));
        // The group's transport and track, from its events when it has
        // reported them, else asked.
        let (state, track) = match heard(&target.coordinator.id) {
            Some(group) if group.transport.is_some() => (
                group.transport.unwrap_or(TransportState::Unknown),
                crate::live::track(&group, Instant::now()),
            ),
            _ => {
                let playback =
                    control::playback(&*self.transport, &households, &target.coordinator.id)
                        .map_err(Failure::from)
                        .inspect_err(|f| self.notice(f))?;
                (
                    playback.transport.state,
                    TrackDto::from_position(&playback.position),
                )
            }
        };
        let volume = heard(&target.player.id)
            .and_then(|room| room.volume)
            .or_else(|| control::volume(&*self.transport, &households, &target.player.id).ok());
        Ok(ZoneStateDto {
            zone: zone_for_target(&households, &target, |_| state),
            transport_state: crate::zones::transport_state_name(state).to_string(),
            volume,
            track,
        })
    }

    /// A group's transport state: from its events when it has reported one,
    /// else asked (`unknown` when the coordinator does not answer).
    fn transport_state(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
    ) -> TransportState {
        self.live()
            .and_then(|live| live.player(coordinator)?.transport)
            .unwrap_or_else(|| {
                control::playback(&*self.transport, households, coordinator)
                    .map_or(TransportState::Unknown, |p| p.transport.state)
            })
    }
}

/// Keep the DJ fed: fold each playback change the live model sees into the
/// DJ's feed for that coordinator, on a thread of its own. Does nothing
/// without a live model or a DJ. Ends with the live model.
pub fn follow(surface: &Arc<Surface>) {
    let (Some(live), true) = (surface.live(), surface.dj.is_some()) else {
        return;
    };
    let changes = live.subscribe();
    let surface = Arc::downgrade(surface);
    let _ = std::thread::Builder::new()
        .name("fsonos-dj".into())
        .spawn(move || {
            while let Ok(change) = changes.recv() {
                let LiveEvent::Playback { player, .. } = change else {
                    continue;
                };
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                surface.dj_playback(&player);
            }
        });
}

/// The coordinators of the groups `command` changes (a join changes both
/// the member's old group and the one it joins).
fn affected(households: &[HouseholdState], command: &Command) -> Vec<PlayerId> {
    let group_of = |p: &PlayerId| {
        households
            .iter()
            .find_map(|h| h.coordinator_of(p).cloned())
            .unwrap_or_else(|| p.clone())
    };
    match command {
        Command::Play { coordinator, .. }
        | Command::PlayFavorite { coordinator, .. }
        | Command::Transport { coordinator, .. }
        | Command::Dj { coordinator, .. } => vec![coordinator.clone()],
        Command::Volume { target, .. } | Command::Mute { target, .. } => vec![group_of(target)],
        Command::Join {
            member,
            coordinator,
        } => vec![group_of(member), coordinator.clone()],
        Command::Leave { member } => vec![group_of(member)],
        Command::Nothing { .. } => Vec::new(),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A [`Surface`] over a canned transport and fixed households, for the
    //! HTTP and MCP surfaces' tests.

    use super::*;
    use fsonos_core::clock::SystemClock;
    use fsonos_proto::ProtoError;
    use std::net::IpAddr;
    use std::sync::Arc;

    /// Answers every SOAP action with success and `out_args`, recording the
    /// action names.
    pub struct Canned {
        pub out_args: &'static str,
        pub sent: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            let action = action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_string();
            self.sent.lock().unwrap().push(action.clone());
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
                self.out_args
            ))
        }
    }

    /// A surface over `households`; returns the SOAP action log too.
    pub fn surface(
        out_args: &'static str,
        households: Vec<HouseholdState>,
    ) -> (Surface, Arc<Mutex<Vec<String>>>) {
        let sent = Arc::new(Mutex::new(Vec::new()));
        let canned = Canned {
            out_args,
            sent: Arc::clone(&sent),
        };
        let survey: Survey = Box::new(move |_| Ok(households.clone()));
        let s = Surface::new(
            Box::new(canned),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        );
        (s, sent)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::surface;
    use super::*;
    use crate::plan::{TransportAction, plan_transport};
    use crate::request::ZoneRequest;
    use crate::zones::fixtures::households;

    #[test]
    fn control_authorizes_plans_and_executes() {
        let (s, sent) = surface("", households());
        let out = s
            .control(&Client::McpStdio, "pause", |h| {
                plan_transport(
                    h,
                    &ZoneRequest {
                        zone: "kitchen@s1".into(),
                    },
                    TransportAction::Pause,
                )
            })
            .unwrap();
        assert_eq!(out.done, "paused Den's group");
        assert_eq!(*sent.lock().unwrap(), ["Pause"]);
        let denied = s
            .control(&Client::Unknown, "pause", |_| unreachable!())
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn zones_and_one_zone_read_live_state() {
        let (s, _) = surface(
            "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>",
            households(),
        );
        let zones = s.zones(&Client::Unknown).unwrap();
        assert_eq!(zones.len(), 4);
        assert!(zones.iter().all(|z| z.transport_state == "paused"));
        let den = s.zone(&Client::Unknown, "Kitchen@S1").unwrap();
        assert_eq!(den.coordinator_room, "Den");
    }

    #[test]
    fn nothing_discovered_is_not_ready() {
        let (s, _) = surface("", Vec::new());
        assert_eq!(
            s.zones(&Client::LoopbackHttp).unwrap_err().code,
            ErrorCode::NotReady
        );
    }
}
