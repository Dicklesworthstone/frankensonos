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
use fsonos_core::rooms::Aliases;
use fsonos_core::snapshot::RestoreOp;
use fsonos_core::store::{
    Action, ActionFilter, LoggedAction, PlayName, PlayRecord, Store, StoreError,
};
use fsonos_core::{HouseholdState, control, moving};
use fsonos_core::{favorites, search};
use fsonos_proto::Transport;
use fsonos_types::{PlayerId, TransportState};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, Weak};
use std::time::{Duration, Instant};

use crate::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto};
use crate::events::{EventBus, ResourceChanges};
use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::guard::{Guard, Note};
use crate::plan::{Command, DjAction, Rooms, plan_play_favorite, resolve};
use crate::reads::{FavoriteDto, HitDto, PlayDto, RoomDto, TrackDto, ZoneStateDto};
use crate::request::{PlayFavoriteRequest, SearchRequest};
use crate::zones::{ZoneDto, zone_for_target, zone_views};

pub mod dj_feedback;
mod dj_follow;
pub mod dj_prefs;
pub mod dj_rehearse;
pub mod dj_sync;
pub mod eq;
pub mod house_policy;
pub mod queue;
#[cfg(test)]
mod relocation_tests;
#[cfg(test)]
mod resource_changes_tests;
pub mod scenes;
pub mod schedules;
pub mod spotify_auth;

pub mod announce;
mod intents;
mod outside;
mod playback_caps;
pub mod players;
mod search_play;
mod shared_store;

pub mod art;

/// Finds the households (a LAN survey, say).
pub type Survey = Box<dyn Fn(&dyn Transport) -> Result<Vec<HouseholdState>, Failure> + Send + Sync>;

/// The owner's room aliases, in the data directory (see
/// `fsonos_core::rooms::Aliases`).
pub const ALIASES_FILE: &str = "aliases.toml";

/// How long a survey answer is reused before the next call surveys again.
pub const REFRESH: Duration = Duration::from_secs(30);

/// How long after a regroup reads survey directly rather than trust the live
/// model, whose topology events may not have arrived yet.
pub const SETTLE: Duration = Duration::from_secs(3);

/// How long a doctor report answers callers that could not be identified
/// before one of them runs the checks again (see [`Surface::doctor`]).
pub const DOCTOR_REUSE: Duration = Duration::from_secs(30);

/// See the module docs.
pub struct Surface {
    transport: Arc<dyn Transport + Send + Sync>,
    survey: Survey,
    cache: Mutex<Option<(Instant, Vec<HouseholdState>)>>,
    /// The house policy in force (see [`Surface::policy`]).
    policy: RwLock<Arc<Policy>>,
    /// `policy.toml`, re-read when it changes ([`Surface::with_policy_file`]).
    policy_file: Option<PolicyFile>,
    clock: Box<dyn Clock>,
    log: Option<ActionLog>,
    doctor_checks: Option<DoctorChecks>,
    /// The last doctor report and when it was made.
    doctor_last: Mutex<Option<(Instant, Report)>>,
    /// Runs the DJ (`dj_start` / `dj_skip` / `dj_stop`); without one they
    /// answer NOT_IMPLEMENTED.
    dj: Option<Box<dyn DjEngine>>,
    /// `aliases.toml`, read on each call that names a room.
    aliases_file: Option<PathBuf>,
    /// The daemon's live model; not kept alive by the surface (see
    /// [`Surface::with_live`]).
    live: Option<Weak<Live>>,
    /// The event stream, fed by the live model and the action log.
    events: Option<Arc<EventBus>>,
    /// The DJ status last published per coordinator (`dj.status`), so an
    /// event goes out only when it changed.
    dj_seen: Mutex<HashMap<PlayerId, DjStatusDto>>,
    /// What the live model heard each coordinator play
    /// ([`Self::record_heard`]).
    heard: Mutex<Heard>,
    /// Until when reads survey directly (set by a regroup).
    settle_until: Mutex<Option<Instant>>,
    /// Clips and where the players fetch them (see [`announce`]).
    announcements: Option<announce::Announcements>,
    /// The daemon's sleep timers ([`Self::with_sleep`]); without them a
    /// sleep timer is the speaker's own, which does not fade.
    sleep: Option<Arc<schedules::Sleep>>,
    /// The owner's Spotify sign-in ([`Self::with_spotify_auth`]); without
    /// it the sign-in routes answer NOT_IMPLEMENTED.
    spotify_auth: Option<Box<dyn spotify_auth::SpotifyAuth>>,
}

/// Doctor checks a surface adds to the core's (the daemon's bind and health
/// checks, say); registered on each run.
pub type DoctorChecks = Box<dyn Fn(&mut Runner) + Send + Sync>;

/// `policy.toml` in a data directory, and its bytes when last read (`None`:
/// there was no file). Compared whole, so an edit that keeps the file's size
/// and modification time is still seen; the file is small.
struct PolicyFile {
    dir: PathBuf,
    seen: Mutex<Option<Vec<u8>>>,
}

impl PolicyFile {
    fn contents(&self) -> Option<Vec<u8>> {
        std::fs::read(self.dir.join(fsonos_core::policy::FILE_NAME)).ok()
    }

    /// Re-read the file into `policy` if it changed since it was last read.
    /// A file that no longer parses is logged and the policy left as it was:
    /// a typo must not lift the owner's caps.
    fn refresh(&self, policy: &RwLock<Arc<Policy>>) {
        let now = self.contents();
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        if *seen == now {
            return;
        }
        *seen = now;
        match Policy::load(&self.dir) {
            Ok(fresh) => {
                // The owner's login comes from the tailnet, not the file.
                let owner = policy
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .owner()
                    .map(str::to_owned);
                *policy.write().unwrap_or_else(PoisonError::into_inner) =
                    Arc::new(fresh.with_owner(owner));
                tracing::info!(
                    "house policy reloaded from {}",
                    fsonos_core::policy::FILE_NAME
                );
            }
            Err(e) => tracing::warn!("the house policy in force is unchanged: {e}"),
        }
    }
}

/// Where a surface logs its actions, and its name in the log.
struct ActionLog {
    store: Mutex<Box<dyn Store + Send>>,
    surface: String,
    /// Held for a whole undo, so two never put the same action back; the
    /// store itself is locked only per call (see [`Surface::undo`]).
    undoing: Mutex<()>,
    /// Shared by every DJ engine call, whole for an undo's restore: the
    /// store no longer keeps the two apart, and a DJ top-up or trim must not
    /// shift a queue the restore checks and then takes tracks out of.
    restoring: RwLock<()>,
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
            policy: RwLock::new(Arc::new(policy)),
            policy_file: None,
            clock,
            log: None,
            doctor_checks: None,
            doctor_last: Mutex::new(None),
            dj: None,
            aliases_file: None,
            live: None,
            events: None,
            dj_seen: Mutex::new(HashMap::new()),
            heard: Mutex::new(Heard::default()),
            settle_until: Mutex::new(None),
            announcements: None,
            sleep: None,
            spotify_auth: None,
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
        self.event_bus().map(Arc::clone)
    }

    /// Observe journal activity for an in-process resource publisher, waiting
    /// at most [`crate::events::MAX_WAIT`]. `None` starts at the current tail.
    /// The result contains cursor and continuity metadata, never event data.
    ///
    /// This is an internal publisher capability, not a client endpoint: the
    /// publisher must authorize each external subscription and document read
    /// separately. It does not impersonate a local or remote caller.
    pub fn resource_changes_after(
        &self,
        since: Option<u64>,
        wait: Duration,
    ) -> Result<ResourceChanges, Failure> {
        Ok(self.event_bus()?.resource_changes_after(since, wait))
    }

    fn event_bus(&self) -> Result<&Arc<EventBus>, Failure> {
        self.events.as_ref().ok_or_else(|| {
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

    /// Resolve rooms with the owner's aliases from `path` (read on each call,
    /// so edits apply at once; a missing file means none).
    #[must_use]
    pub fn with_aliases_file(mut self, path: PathBuf) -> Self {
        self.aliases_file = Some(path);
        self
    }

    /// The data directory this surface follows `policy.toml` in
    /// ([`Self::with_policy_file`]): the daemon's.
    #[must_use]
    pub fn data_dir(&self) -> Option<&Path> {
        self.policy_file.as_ref().map(|file| file.dir.as_path())
    }

    /// Follow `policy.toml` in `data_dir`: each call reads the policy anew
    /// when the file has changed (a missing file is the defaults), so an
    /// edited cap takes hold without restarting the daemon. The policy given
    /// to [`Surface::new`] stands until the file changes.
    #[must_use]
    pub fn with_policy_file(mut self, data_dir: PathBuf) -> Self {
        let file = PolicyFile {
            dir: data_dir,
            seen: Mutex::new(None),
        };
        *file.seen.lock().unwrap_or_else(PoisonError::into_inner) = file.contents();
        self.policy_file = Some(file);
        self
    }

    /// The house policy in force now, re-read first when its file has
    /// changed ([`Self::with_policy_file`]).
    #[must_use]
    pub fn policy(&self) -> Arc<Policy> {
        if let Some(file) = &self.policy_file {
            file.refresh(&self.policy);
        }
        Arc::clone(&self.policy.read().unwrap_or_else(PoisonError::into_inner))
    }

    /// The owner's aliases as the file says now. A malformed file is logged
    /// and ignored (the doctor reports it) rather than failing every call.
    pub fn aliases(&self) -> Option<Aliases> {
        let path = self.aliases_file.as_ref()?;
        match Aliases::load(path) {
            Ok(aliases) => Some(aliases),
            Err(e) => {
                tracing::warn!("room aliases ignored: {e}");
                None
            }
        }
    }

    /// Run `dj_start` / `dj_skip` / `dj_stop` with `engine`; with a live
    /// model, [`follow`] keeps its queues fed.
    #[must_use]
    pub fn with_dj(mut self, engine: Box<dyn DjEngine>) -> Self {
        self.dj = Some(engine);
        self
    }

    /// Publish the DJ's status in the group `at.coordinator` leads as a
    /// `dj.status` event (what it plays, what it picked next and why, its
    /// steering), when it changed since the last one. Only a surface with
    /// an event stream does anything.
    fn publish_dj(
        &self,
        dj: &dyn DjEngine,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        queue_position: Option<u32>,
    ) {
        let Some(bus) = &self.events else {
            return;
        };
        // One publisher at a time, from status to seen: a status computed
        // before a stop must not go out after it.
        let mut seen = self.dj_seen.lock().unwrap_or_else(PoisonError::into_inner);
        let Ok(status) = dj.status(at, store, queue_position, &*self.clock) else {
            return;
        };
        if seen.get(at.coordinator) == Some(&status) {
            return;
        }
        if let Ok(serde_json::Value::Object(mut data)) = serde_json::to_value(&status) {
            data.insert("player".into(), at.coordinator.0.clone().into());
            bus.publish("dj.status", data.into());
        }
        seen.insert(at.coordinator.clone(), status);
    }

    /// Fold `player`'s latest playback into the DJ's feed, if it feeds that
    /// coordinator (see [`follow`]), and publish what changed.
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
        let fed = self.with_dj_store(|store| {
            dj.on_playback(at, store, &playback, &*self.clock);
            self.publish_dj(&**dj, at, store, playback.queue_position);
            Ok(())
        });
        // The DJ logs its own top-ups; announce them as any action is.
        self.announce_logged(&dj.take_logged());
        if let Ok(None) = fed {
            tracing::warn!("the DJ needs the daemon's store to record and top up");
        }
    }

    /// Record what `player` plays now, as the live model hears it, so plays
    /// started in the Sonos app are history too: a coordinator's new track
    /// once it plays, as a `spotify:` URI when it is Spotify. A pause and
    /// resume, or a station's next song title, is the same play; a stop ends
    /// it. A group member (its coordinator's `x-rincon:` stream) and TV
    /// audio record nothing: what the room played ended. Music that moved
    /// here from another room is the play it was there, the play recorded
    /// just now in any room (by the DJ or a command) is this one, and so is
    /// a room's last play when this is the first the daemon hears of it (it
    /// was playing before the daemon started).
    fn record_heard(&self, player: &PlayerId) {
        let Some(playback) = self.live().and_then(|live| live.player(player)) else {
            return;
        };
        let mut heard = self.heard.lock().unwrap_or_else(PoisonError::into_inner);
        let raw = playback.track_uri.as_deref().filter(|u| !u.is_empty());
        let no_music = |u: &str| u.starts_with("x-rincon:") || u.starts_with("x-sonos-htastream:");
        if playback.transport == Some(TransportState::Stopped) || raw.is_some_and(no_music) {
            heard.plays(player, None);
            return;
        }
        let Some(raw) = raw.filter(|_| playback.transport == Some(TransportState::Playing)) else {
            return;
        };
        // Spotify Connect names its session between tracks (seen live): no
        // track, and not the end of the one heard.
        if raw.starts_with("x-sonos-vli:") && raw.contains(",spotify:") {
            return;
        }
        let uri = played_uri(raw);
        let Some(first) = heard.plays(player, Some(uri.clone())) else {
            return;
        };
        let moved = heard.elsewhere(player, &uri);
        drop(heard);
        if moved {
            return;
        }
        // What the player calls it, for the history to show when the
        // library does not know it (a stream, a track outside the library).
        let name = (playback.now_playing.as_ref())
            .filter(|n| !n.title.trim().is_empty() && n.title != raw && n.title != uri)
            .map(|n| PlayName {
                source_uri: uri.clone(),
                title: n.title.trim().to_string(),
                artist: n.creator.clone().filter(|c| !c.trim().is_empty()),
            });
        let at = self.now();
        let recorded = self.with_store(|s| {
            if let Some(name) = &name {
                s.name_play(name, at)?;
            }
            let same = |p: &PlayRecord| played_uri(&p.source_uri) == uri;
            let left = first && s.recent_plays(Some(player.0.as_str()), 1)?.iter().any(same);
            let just = (s.recent_plays(None, RECENT_PLAYS)?.iter())
                .any(|p| same(p) && at - p.played_at < JUST_RECORDED);
            if left || just {
                return Ok(());
            }
            s.record_play(&player.0, &uri, at)
        });
        if let Err(f) = recorded {
            tracing::warn!("a play heard was not recorded ({uri}): {}", f.detail);
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
    ///
    /// The LAN checks search the network, read every player and subscribe to
    /// their events, so a caller that could not be identified gets the last
    /// report while it is younger than [`DOCTOR_REUSE`], and such callers run
    /// the checks one at a time. Everyone else gets a fresh run.
    pub fn doctor(&self, client: &Client) -> Result<Report, Failure> {
        self.guard(client).authorize("doctor", true)?;
        if *client != Client::Unknown {
            let report = self.run_doctor()?;
            // Never wait for an unidentified caller's run to keep this one.
            if let Ok(mut last) = self.doctor_last.try_lock() {
                *last = Some((Instant::now(), report.clone()));
            }
            return Ok(report);
        }
        let mut last = self
            .doctor_last
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some((at, report)) = &*last
            && at.elapsed() < DOCTOR_REUSE
        {
            return Ok(report.clone());
        }
        let report = self.run_doctor()?;
        *last = Some((Instant::now(), report.clone()));
        Ok(report)
    }

    fn run_doctor(&self) -> Result<Report, Failure> {
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
            undoing: Mutex::new(()),
            restoring: RwLock::new(()),
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
            Ok(id) => self.announce_logged(&[LoggedAction { id, action }]),
            Err(e) => tracing::warn!("action not logged ({}): {e}", action.intent),
        }
    }

    /// Announce logged actions on the event stream (`action.logged`).
    fn announce_logged(&self, logged: &[LoggedAction]) {
        let Some(bus) = &self.events else {
            return;
        };
        for row in logged {
            if let Ok(data) = serde_json::to_value(crate::log::ActionDto::from(row)) {
                bus.publish("action.logged", data);
            }
        }
    }

    fn guard<'a>(&'a self, client: &'a Client) -> Guard<'a> {
        Guard {
            policy: self.policy(),
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

    /// Carry `command` out: the rooms it starts music in held to the caller's
    /// caps first ([`Self::lower_before_playing`]), then the command, retried
    /// once where the speakers changed under it ([`crate::heal`]).
    fn execute(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        let mut notes = self.lower_before_playing(households, guard, command)?;
        let mut outcome = self.carry_out(households, guard, command)?;
        notes.append(&mut outcome.notes);
        outcome.notes = notes;
        Ok(outcome)
    }

    /// [`Self::execute`] once the music it starts is within the caps: the DJ,
    /// or the command itself, healed when the speakers changed under it.
    fn carry_out(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        if let Command::Dj { coordinator, .. } | Command::DjSteer { coordinator, .. } = command
            && let Some(dj) = &self.dj
        {
            let at = DjSpeakers {
                transport: &*self.transport,
                households,
                coordinator,
            };
            return self
                .with_dj_store(|store| {
                    let done = run_dj(&**dj, at, store, command, &*self.clock);
                    let queue_position = self
                        .live()
                        .and_then(|live| live.player(coordinator)?.queue_position);
                    self.publish_dj(&**dj, at, store, queue_position);
                    Ok(done)
                })?
                .unwrap_or_else(|| Err(no_dj_store()));
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
        plan: impl FnOnce(&Rooms<'_>) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        self.control_io(client, tool, |_, rooms| plan(rooms))
    }

    /// [`Self::control`] for plans that read from the speakers first (a
    /// household's favorites, say); the plan gets the transport too.
    pub fn control_io(
        &self,
        client: &Client,
        tool: &str,
        plan: impl FnOnce(&dyn Transport, &Rooms<'_>) -> Result<Command, Failure>,
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
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let command = plan(&*self.transport, &rooms).map_err(|f| self.explain(f))?;
        // A volume or mute change ends a sleep fade in flight on the group, so
        // the fade does not override the new level (the timer ends where it is).
        if let Some(sleep) = &self.sleep
            && matches!(command, Command::Volume { .. } | Command::Mute { .. })
        {
            for coordinator in affected(&households, &command) {
                sleep.volume_changed(&households, &coordinator);
            }
        }
        let regroups = regroups(&command);
        // Already satisfied requests change nothing and are not logged.
        if self.log.is_none() || matches!(command, Command::Nothing { .. }) {
            let result = self.execute_following(&households, &guard, &command);
            if regroups {
                self.invalidate();
            }
            if let Err(f) = &result {
                self.notice(f);
            }
            return result;
        }
        let intent = intents::of(tool, &households, std::slice::from_ref(&command));
        let (snaps, missed) = actions::capture_zones(
            &*self.transport,
            &households,
            &affected(&households, &command),
            self.now(),
        );
        let sessions = self.sessions_before(&command);
        let result = self.execute_following(&households, &guard, &command);
        if regroups {
            self.invalidate();
        }
        if let Err(f) = &result {
            self.notice(f);
        }
        let before = actions::before_state_with(&snaps, &sessions);
        let (decision, text, before) = log_entry(&result, before, missed.len());
        let before = self.with_after(before, &snaps, &[]);
        self.record(client, intent, decision, text, before);
        result
    }

    /// Like [`Self::control_io`] but for commands that act on a *set* of
    /// rooms — `pause`/`resume`/`next`/`previous`, `mute` and `volume` given
    /// `all`/`everywhere` or a multi-room alias. The plan resolves to one
    /// command per room or group; they run under one authorization, share one
    /// before-state, and record as a single action, so `undo` restores every
    /// affected room together.
    pub fn control_set(
        &self,
        client: &Client,
        tool: &str,
        plan: impl FnOnce(&Rooms<'_>) -> Result<Vec<Command>, Failure>,
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
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let commands = plan(&rooms).map_err(|f| self.explain(f))?;
        // A volume or mute change ends a sleep fade in flight on each affected
        // group, so the fade does not override the new level.
        if let Some(sleep) = &self.sleep {
            for command in &commands {
                if matches!(command, Command::Volume { .. } | Command::Mute { .. }) {
                    for coordinator in affected(&households, command) {
                        sleep.volume_changed(&households, &coordinator);
                    }
                }
            }
        }
        // Already-satisfied rooms change nothing and are dropped.
        let commands: Vec<Command> = commands
            .into_iter()
            .filter(|c| !matches!(c, Command::Nothing { .. }))
            .collect();
        if commands.is_empty() {
            return Ok(OutcomeDto {
                done: "already so; nothing to do".to_string(),
                changed: false,
                volume: None,
                notes: Vec::new(),
            });
        }
        // `ungroup everywhere`: the cached zones are stale once it has run.
        let regroups = commands.iter().any(regroups);
        // No action log: the per-command path is enough.
        if self.log.is_none() {
            let outcome = self.execute_set(&households, &guard, &commands);
            if regroups {
                self.invalidate();
            }
            if let Err(f) = &outcome {
                self.notice(f);
            }
            return outcome;
        }
        let intent = intents::of(tool, &households, &commands);
        // One before-state across every affected zone, so one undo restores
        // the whole set.
        let mut targets: Vec<PlayerId> = Vec::new();
        for command in &commands {
            for player in affected(&households, command) {
                if !targets.contains(&player) {
                    targets.push(player);
                }
            }
        }
        let (snaps, missed) =
            actions::capture_zones(&*self.transport, &households, &targets, self.now());
        let result = self.execute_set(&households, &guard, &commands);
        if regroups {
            self.invalidate();
        }
        if let Err(f) = &result {
            self.notice(f);
        }
        let before = actions::before_state_with(&snaps, &[]);
        let (decision, text, before) = log_entry(&result, before, missed.len());
        let before = self.with_after(before, &snaps, &[]);
        self.record(client, intent, decision, text, before);
        result
    }

    /// `before`, an undoable action's before-state, with the touched rooms
    /// (every member of `snaps`' zones) as the action left them, so undo
    /// can tell whether someone changed them since. `levels` stands in for
    /// a room's volume while its speaker still ramps there itself (a fade
    /// with no length): read now, it is mid-ramp, so the level it ramps
    /// from is kept too. No reads without an action log or a before-state.
    fn with_after(
        &self,
        before: Option<String>,
        snaps: &[fsonos_core::snapshot::ZoneSnapshot],
        levels: &[(PlayerId, u8)],
    ) -> Option<String> {
        if self.log.is_none() {
            return before;
        }
        before.as_ref()?;
        let rooms: Vec<PlayerId> = snaps.iter().flat_map(|s| s.members.clone()).collect();
        let Ok(households) = self.households() else {
            return before;
        };
        let mut after = actions::rooms_after(&*self.transport, &households, &rooms);
        for room in &mut after {
            if let Some(&(_, level)) = levels.iter().find(|(p, _)| *p == room.player) {
                room.ramp_from = snaps
                    .iter()
                    .flat_map(|s| &s.levels)
                    .find(|l| l.player == room.player)
                    .map(|l| l.volume);
                room.volume = level;
            }
        }
        actions::with_after(before, &after)
    }

    /// Run each command of a set and fold the outcomes into one: the
    /// sentences joined, the notes merged, and `changed` true if any room
    /// changed. A room or group whose speaker fails ([`speaker_side`]), or
    /// that the policy refuses (a raise of a room already at its cap), is
    /// noted as SKIPPED and the rest go ahead: one offline or idle group
    /// does not stop `pause everywhere`, and a room at its cap does not turn
    /// another room's change into a "denied" call that undo cannot reach.
    /// The call fails when none went ahead, or at the first failure of any
    /// other kind.
    fn execute_set(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        commands: &[Command],
    ) -> Result<OutcomeDto, Failure> {
        let mut combined = OutcomeDto {
            done: String::new(),
            changed: false,
            volume: None,
            notes: Vec::new(),
        };
        let (mut skipped, mut went) = (None, false);
        for command in commands {
            let outcome = match self.execute_following(households, guard, command) {
                Ok(outcome) => outcome,
                Err(failure)
                    if commands.len() > 1
                        && (speaker_side(&failure) || failure.code == ErrorCode::PolicyDenied) =>
                {
                    let room = command
                        .addressed()
                        .and_then(|id| households.iter().find_map(|h| h.player(id)))
                        .map_or("a room", |p| p.room_name.as_str());
                    combined.notes.push(Note {
                        code: NoteCode::Skipped,
                        detail: format!("{room} skipped: {}", failure.detail),
                    });
                    skipped.get_or_insert(failure);
                    continue;
                }
                Err(failure) => return Err(failure),
            };
            went = true;
            if !combined.done.is_empty() && !outcome.done.is_empty() {
                combined.done.push_str("; ");
            }
            combined.done.push_str(&outcome.done);
            combined.changed |= outcome.changed;
            combined.volume = combined.volume.or(outcome.volume);
            combined.notes.extend(outcome.notes);
        }
        match skipped {
            Some(failure) if !went => Err(failure),
            _ => Ok(combined),
        }
    }

    /// Undo the newest undoable action — only `client`'s own when
    /// `own_only` — and log the undo. `Ok(None)` when there is nothing to
    /// undo.
    pub fn undo(&self, client: &Client, own_only: bool) -> Result<Option<UndoReport>, Failure> {
        self.undo_forced(client, own_only, false)
    }

    /// [`Self::undo`], putting back even the zones someone changed since
    /// the action when `force`: the owner's CLI alone may.
    pub fn undo_forced(
        &self,
        client: &Client,
        own_only: bool,
        force: bool,
    ) -> Result<Option<UndoReport>, Failure> {
        if force && *client != Client::Cli {
            return Err(Failure::new(
                ErrorCode::PolicyDenied,
                "only the owner's CLI may force an undo",
            ));
        }
        let Some(log) = &self.log else {
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                "undo needs the action log, which this surface does not keep",
            ));
        };
        let guard = self.guard(client);
        if let Err(denied) = guard.authorize("undo_last", false) {
            let detail = denied.detail.clone();
            self.record(
                client,
                "undo_last".into(),
                format!("deny: {detail}"),
                detail,
                None,
            );
            return Err(denied);
        }
        // Only the CLI undoes someone else's action: every other caller
        // undoes its own newest (README, "the caller's own newest").
        let own_only = own_only || *client != Client::Cli;
        // Plan the restore against the speakers as they are now.
        self.invalidate();
        let households = self.households()?;
        let vet = |op: &RestoreOp| {
            let vetted = vet_restore(&guard, &*self.transport, &households, op);
            if let Ok(RestoreOp::SetVolume { player, .. }) = &vetted {
                self.volumes_changing(&households, std::slice::from_ref(player));
            }
            vetted
        };
        // One undo at a time, but the store is locked only for each read or
        // write: the restore talks to the speakers, and every other call's
        // log entry must not wait on it.
        let _undoing = log.undoing.lock().unwrap_or_else(PoisonError::into_inner);
        let _restoring = log
            .restoring
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let report = actions::undo_last_vetted(
            &*self.transport,
            &households,
            &mut shared_store::SharedStore(&log.store),
            own_only.then_some(client),
            client,
            &log.surface,
            self.now(),
            force,
            &vet,
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

    /// Every room with its household, the zone it plays in, and the
    /// owner's aliases that name it (`list_rooms`, read-only).
    pub fn rooms(&self, client: &Client) -> Result<Vec<RoomDto>, Failure> {
        self.guard(client).authorize("list_rooms", true)?;
        let households = self.households()?;
        let labels = fsonos_core::rooms::household_labels(&households);
        let aliases = self.aliases();
        let named = |primary: &PlayerId| -> Vec<String> {
            aliases
                .iter()
                .flat_map(Aliases::aliases)
                .filter(|alias| {
                    alias.rooms.iter().any(|room| {
                        fsonos_core::resolve_room(&households, room)
                            .is_ok_and(|t| t.room.primary == *primary)
                    })
                })
                .map(|alias| alias.name.clone())
                .collect()
        };
        Ok(households
            .iter()
            .zip(&labels)
            .flat_map(|(h, label)| {
                h.rooms.iter().map(|room| RoomDto {
                    name: room.name.clone(),
                    household: label.clone(),
                    zone: h
                        .rooms
                        .iter()
                        .find(|r| r.players.contains(&room.coordinator))
                        .map_or_else(|| room.name.clone(), |r| r.name.clone()),
                    aliases: named(&room.primary),
                })
            })
            .collect())
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
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), room)
            .map_err(|f| self.explain(f))?;
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
                favorites::list(transport, households.households, &target.coordinator.id)?;
            plan_play_favorite(households, req, &household_favorites)
        })
    }

    /// Search the owner's saved library (tracks, saved albums and their
    /// playlists) and, with a zone, its household's Sonos favorites
    /// (`search_library`, read-only). Without a store there is no library,
    /// only favorites.
    pub fn search_library(
        &self,
        client: &Client,
        req: &SearchRequest,
    ) -> Result<Vec<HitDto>, Failure> {
        self.guard(client).authorize("search_library", true)?;
        let (query, limit) = (req.query()?, req.limit()?);
        let library = self.with_store(|s| s.library())?.unwrap_or_default();
        let playlists = self.with_store(|s| s.playlists())?.unwrap_or_default();
        let household_favorites = match req.zone()? {
            Some(zone) => {
                let households = self.households()?;
                let aliases = self.aliases();
                let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
                    .map_err(|f| self.explain(f))?;
                favorites::list(&*self.transport, &households, &target.coordinator.id)?
            }
            None => Vec::new(),
        };
        Ok(
            search::search_all(&library, &playlists, &household_favorites, query, limit)
                .iter()
                .map(HitDto::from)
                .collect(),
        )
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
        let aliases = self.aliases();
        let key = zone
            .map(|z| {
                resolve(room_view(&households, aliases.as_ref(), client), z)
                    .map(|t| t.coordinator.id.0.clone())
            })
            .transpose()
            .map_err(|f| self.explain(f))?;
        let plays = self
            .with_store(|s| s.recent_plays(key.as_deref(), limit))?
            .unwrap_or_default();
        let names = self
            .with_store(|s| crate::reads::play_names(&*s, &plays))?
            .unwrap_or_default();
        Ok(plays
            .iter()
            .rev()
            .map(|p| {
                let (title, artist) = names.get(&p.source_uri).cloned().unwrap_or_default();
                PlayDto {
                    room: control::locate(&households, &PlayerId(p.zone.clone()))
                        .ok()
                        .map(|player| player.room_name.clone()),
                    source_uri: p.source_uri.clone(),
                    title,
                    artist,
                    played_at: p.played_at,
                }
            })
            .collect())
    }

    /// Run `f` on the store, if the surface keeps one (with its action log).
    /// Never a DJ engine call: those go through [`Self::with_dj_store`].
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
        let Some(uri) = uri.map(played_uri) else {
            return;
        };
        let at = self.now();
        // The heard history may have written this play from its event first.
        let recorded = self.with_store(|s| {
            if shared_store::recorded_just_now(&*s, &coordinator.0, &uri, at)? {
                return Ok(());
            }
            s.record_play(&coordinator.0, &uri, at)
        });
        if let Err(e) = recorded {
            tracing::warn!("play not recorded ({uri}): {}", e.detail);
        }
    }

    /// [`Self::execute`], recording a play, and with the DJ held across a
    /// move ([`Self::dj_hold`]) and then following the music
    /// ([`Self::dj_follows`]), as the sleep timer does.
    fn execute_following(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
    ) -> Result<OutcomeDto, Failure> {
        let held = self.dj_hold(households, command);
        let result = self.execute(households, guard, command);
        if result.is_ok() {
            self.remember_play(command);
        }
        // A successful no-op did not transfer any music. Release the hold,
        // but keep DJ steering, feeds and timers on their original groups.
        let moved = result.as_ref().is_ok_and(|outcome| outcome.changed);
        self.dj_follows(households, command, held, moved);
        self.sleep_follows(households, command, moved);
        result
    }

    /// Before a move, hold the DJ in the group the music leaves (see
    /// [`DjEngine::moving`]): the old coordinator's events while its queue is
    /// handed over must not make the DJ let go. Returns that coordinator.
    fn dj_hold(&self, before: &[HouseholdState], command: &Command) -> Option<PlayerId> {
        let Command::Move { from, .. } = command else {
            return None;
        };
        let old = before
            .iter()
            .find_map(|h| h.coordinator_of(from))
            .cloned()?;
        if let Some(dj) = &self.dj {
            dj.moving(&old);
        }
        Some(old)
    }

    /// After a move (`moved`: it succeeded), the DJ follows the music: when
    /// the group plays on under another coordinator (handed over, or
    /// replayed there), its stored session and the DJ's feed move there too.
    /// Either way the DJ held by [`Self::dj_hold`] acts on playback again.
    /// `before` is the speakers as they were; the new coordinator comes from
    /// the target's own topology.
    fn dj_follows(
        &self,
        before: &[HouseholdState],
        command: &Command,
        held: Option<PlayerId>,
        moved: bool,
    ) {
        let (Some(old), Command::Move { to, .. }) = (held, command) else {
            return;
        };
        let new = if moved {
            moving::coordinator_now(&*self.transport, before, to).unwrap_or_else(|e| {
                tracing::warn!("the DJ stays with {}: {e}", old.0);
                old.clone()
            })
        } else {
            old.clone()
        };
        if new == old {
            if let Some(dj) = &self.dj {
                dj.moved(&old, &old);
            }
            return;
        }
        // A DJ that moved with no steering of its own leaves none behind it:
        // the target's old session was for music it no longer plays. With no
        // DJ running, the target keeps its standing steering.
        let feeding = self.dj.as_ref().is_some_and(|dj| dj.feeds(&old));
        let rekeyed = self.with_store(|s| {
            if moving::rekey_dj_session(s, &old, &new)? || !feeding {
                return Ok(());
            }
            s.delete_dj_session(&new.0)
        });
        if let Err(f) = rekeyed {
            tracing::warn!("the DJ's steering stays with {}: {}", old.0, f.detail);
        }
        if let Some(dj) = &self.dj {
            dj.moved(&old, &new);
        }
    }

    /// The DJ sessions `command` replaces (a steer's group's), as they are
    /// before it runs, so undo can put them back.
    fn sessions_before(&self, command: &Command) -> Vec<actions::SessionBefore> {
        let Command::DjSteer { coordinator, .. } = command else {
            return Vec::new();
        };
        let captured = self.with_store(|store| {
            Ok(actions::capture_sessions(
                &*store,
                std::slice::from_ref(coordinator),
            ))
        });
        match captured {
            Ok(Some(Ok(sessions))) => sessions,
            Ok(None) => Vec::new(),
            Ok(Some(Err(e))) => {
                tracing::warn!("the steering it replaces is not kept for undo: {e}");
                Vec::new()
            }
            Err(f) => {
                tracing::warn!(
                    "the steering it replaces is not kept for undo: {}",
                    f.detail
                );
                Vec::new()
            }
        }
    }

    /// The DJ engine, or why there is none.
    fn dj_engine(&self) -> Result<&dyn DjEngine, Failure> {
        self.dj.as_deref().ok_or_else(|| {
            Failure::new(ErrorCode::NotImplemented, "this surface runs no DJ")
                .with_hint("Ask the daemon (fsonos serve), which runs the DJ.")
        })
    }

    /// What the DJ plays in the zone `zone` plays in, why, what comes next,
    /// and how its next pick is steered (`dj_status`).
    pub fn dj_status(&self, client: &Client, zone: &str) -> Result<DjStatusDto, Failure> {
        self.guard(client).authorize("dj_status", true)?;
        let dj = self.dj_engine()?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let coordinator = &target.coordinator.id;
        // Where the group's queue is, which places the movement playing.
        let queue_position = if dj.feeds(coordinator) {
            self.live()
                .and_then(|live| live.player(coordinator)?.queue_position)
                .or_else(|| {
                    control::playback(&*self.transport, &households, coordinator)
                        .ok()
                        .map(|p| p.position.track)
                        .filter(|&track| track > 0)
                })
        } else {
            None
        };
        let at = DjSpeakers {
            transport: &*self.transport,
            households: &households,
            coordinator,
        };
        self.with_dj_store(|store| Ok(dj.status(at, store, queue_position, &*self.clock)))?
            .unwrap_or_else(|| Err(no_dj_store()))
    }

    /// The DJ's moods and time-of-day programs, and the steering in effect
    /// now: in the zone `zone` plays in, else the house's program
    /// (`dj_moods`).
    pub fn dj_moods(&self, client: &Client, zone: Option<&str>) -> Result<DjMoodsDto, Failure> {
        self.guard(client).authorize("dj_moods", true)?;
        let dj = self.dj_engine()?;
        let Some(zone) = zone else {
            return self
                .with_dj_store(|store| Ok(dj.moods(None, store, &*self.clock)))?
                .unwrap_or_else(|| Err(no_dj_store()));
        };
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let at = DjSpeakers {
            transport: &*self.transport,
            households: &households,
            coordinator: &target.coordinator.id,
        };
        self.with_dj_store(|store| Ok(dj.moods(Some(at), store, &*self.clock)))?
            .unwrap_or_else(|| Err(no_dj_store()))
    }

    /// The favorites of the household `zone` belongs to (`list_favorites`).
    pub fn favorites(&self, client: &Client, zone: &str) -> Result<Vec<FavoriteDto>, Failure> {
        self.guard(client).authorize("list_favorites", true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let listed = favorites::list(&*self.transport, &households, &target.coordinator.id)?;
        Ok(listed.iter().map(FavoriteDto::from).collect())
    }

    /// What `zone` is doing right now (`get_zone_state`): its group, the
    /// group's transport and track, the room's own volume and mute, and the
    /// group's volume when the room plays with others.
    pub fn zone_state(&self, client: &Client, zone: &str) -> Result<ZoneStateDto, Failure> {
        self.guard(client).authorize("get_zone_state", true)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
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
        let mute = heard(&target.player.id)
            .and_then(|room| room.mute)
            .or_else(|| fsonos_proto::control::get_mute(&*self.transport, target.player.ip).ok());
        let zone = zone_for_target(&households, &target, |_| state);
        // The group's level, when the room plays with others: from its
        // events when it has reported one, else asked.
        let group_volume = if zone.members.len() > 1 {
            heard(&target.coordinator.id)
                .and_then(|group| group.group_volume)
                .or_else(|| {
                    fsonos_proto::control::get_group_volume(&*self.transport, target.coordinator.ip)
                        .ok()
                })
        } else {
            None
        };
        // Shuffle and repeat: from the group's events when it has reported
        // its mode, else asked.
        let mode = heard(&target.coordinator.id)
            .and_then(|group| group.play_mode)
            .and_then(|mode| fsonos_types::PlayMode::from_wire(&mode))
            .or_else(|| {
                fsonos_proto::play_mode::get_play_mode(&*self.transport, target.coordinator.ip).ok()
            });
        let (shuffle, repeat) = mode
            .map(crate::zones::shuffle_and_repeat)
            .map_or((None, None), |(shuffle, repeat)| {
                (Some(shuffle), Some(repeat.to_string()))
            });
        Ok(ZoneStateDto {
            zone,
            transport_state: crate::zones::transport_state_name(state).to_string(),
            volume,
            mute,
            group_volume,
            shuffle,
            repeat,
            play_mode: mode.map(|mode| mode.as_str().to_owned()),
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

/// `households` with `aliases`, for `client`'s requests.
fn room_view<'a>(
    households: &'a [HouseholdState],
    aliases: Option<&'a Aliases>,
    client: &'a Client,
) -> Rooms<'a> {
    Rooms {
        households,
        aliases,
        client: Some(client.key()),
    }
}

/// How long ago a play the DJ or a command recorded still counts as the
/// one the live model then hears, in seconds.
const JUST_RECORDED: i64 = 120;

/// How many of the house's latest plays a play heard is checked against.
const RECENT_PLAYS: usize = 16;

/// How long after a room stops playing something another room that plays
/// it is still the same play, moved there.
const MOVED_WITHIN: Duration = Duration::from_secs(120);

/// What the live model heard each coordinator play, so the history records
/// a play once ([`Surface::record_heard`]).
#[derive(Default)]
struct Heard {
    /// The track each coordinator was last heard playing, `None` once it
    /// played none; one missing was not heard since the daemon started.
    now: HashMap<PlayerId, Option<String>>,
    /// Tracks a coordinator stopped being heard playing, and when.
    ended: Vec<(PlayerId, String, Instant)>,
}

impl Heard {
    /// `player` plays `uri` now (`None`: no music of its own), and what it
    /// played before ended there. `None` when it played `uri` already;
    /// else whether this is the first the daemon hears of it.
    fn plays(&mut self, player: &PlayerId, uri: Option<String>) -> Option<bool> {
        let before = self.now.get(player);
        if before.is_some_and(|b| *b == uri) {
            return None;
        }
        let first = before.is_none();
        if let Some(Some(was)) = self.now.insert(player.clone(), uri) {
            self.ended.push((player.clone(), was, Instant::now()));
        }
        self.ended.retain(|(.., at)| at.elapsed() < MOVED_WITHIN);
        Some(first)
    }

    /// Whether another coordinator plays `uri`, or did until just now: the
    /// music moved to `player`.
    fn elsewhere(&self, player: &PlayerId, uri: &str) -> bool {
        self.now
            .iter()
            .any(|(p, u)| p != player && u.as_deref() == Some(uri))
            || (self.ended.iter())
                .any(|(p, u, at)| p != player && u == uri && at.elapsed() < MOVED_WITHIN)
    }
}

/// `uri` as the history keeps it: a Spotify track, album or playlist as its
/// `spotify:` URI (a renderer's `x-sonos-spotify:` form, a favorite's
/// container), anything else as it is.
fn played_uri(uri: &str) -> String {
    fsonos_proto::didl::spotify_uri_from_renderer_uri(uri).unwrap_or_else(|| uri.to_string())
}

/// Keep the DJ fed from live playback events and periodic cached-state
/// maintenance, and the history whole, each on a thread of its own.
/// Restored feeds are visited at once; quiet queues retry without needing a
/// new event. Ends with the live model or surface. The DJ's follower needs
/// a DJ; the history needs only a live model.
pub fn follow(surface: &Arc<Surface>) {
    dj_follow::follow(surface);
    follow_heard(surface);
}

/// Record each coordinator's new track as played ([`Surface::record_heard`])
/// from the live model's playback changes, on a thread of its own, whether
/// or not there's a DJ. Ends with the live model or the surface.
fn follow_heard(surface: &Arc<Surface>) {
    let Some(live) = surface.live() else {
        return;
    };
    let changes = live.subscribe();
    let surface = Arc::downgrade(surface);
    if let Err(error) = std::thread::Builder::new()
        .name("fsonos-heard".into())
        .spawn(move || {
            while let Ok(change) = changes.recv() {
                let Some(surface) = surface.upgrade() else {
                    return;
                };
                if let LiveEvent::Playback { player, .. } = change {
                    surface.record_heard(&player);
                }
            }
        })
    {
        tracing::error!(%error, "could not start the history follower");
    }
}

/// Carry a DJ command out with `dj`: a steer, and then with `start` a start.
fn run_dj(
    dj: &dyn DjEngine,
    at: DjSpeakers<'_>,
    store: &mut dyn Store,
    command: &Command,
    clock: &dyn Clock,
) -> Result<OutcomeDto, Failure> {
    match command {
        Command::Dj { action, .. } => dj.act(at, store, *action, clock),
        Command::DjSteer { steer, start, .. } => {
            let steered = dj.steer(at, store, steer, clock)?;
            if !start {
                return Ok(steered);
            }
            let started = dj.act(at, store, DjAction::Start, clock)?;
            Ok(OutcomeDto {
                done: format!("{}; {}", steered.done, started.done),
                ..started
            })
        }
        _ => Err(Failure::new(
            ErrorCode::Internal,
            format!("not a DJ command: {command:?}"),
        )),
    }
}

/// A failure at one speaker (it did not answer, or answered with a fault),
/// which a set notes and steps past rather than stopping for.
fn speaker_side(failure: &Failure) -> bool {
    matches!(
        failure.code,
        ErrorCode::PlayerUnreachable | ErrorCode::UpnpFault | ErrorCode::NotCoordinator
    )
}

/// What an undo by `guard`'s caller may put back of `op`: each step is
/// authorized as the tool it stands for, and a volume is bounded by the
/// caller's cap and quiet hours as if it asked for that level now, so an
/// undo is never a way round the house policy.
fn vet_restore(
    guard: &Guard<'_>,
    transport: &dyn Transport,
    households: &[HouseholdState],
    op: &RestoreOp,
) -> Result<RestoreOp, String> {
    let tool = match op {
        RestoreOp::SetVolume { .. } => "set_volume",
        RestoreOp::SetMute { .. } => "mute",
        RestoreOp::Join { .. } => "group",
        RestoreOp::Leave { .. } => "ungroup",
        RestoreOp::Stop => "pause",
        RestoreOp::Play | RestoreOp::PlayQueue => "resume",
        RestoreOp::SetUri { .. } | RestoreOp::SeekTrack(_) | RestoreOp::SeekPosition(_) => "play",
    };
    guard.authorize(tool, false).map_err(|f| f.detail)?;
    let RestoreOp::SetVolume { player, level } = op else {
        return Ok(op.clone());
    };
    // Bounded as the room the player is in now: it may no longer lead it (a
    // pair's other half, say), and a cap applies by room.
    let room = households
        .iter()
        .flat_map(|h| &h.rooms)
        .find(|r| r.players.contains(player))
        .ok_or_else(|| format!("{} plays in no room now", player.0))?;
    let asked = Command::Volume {
        target: room.primary.clone(),
        scope: crate::plan::VolumeScope::Room,
        change: crate::request::VolumeChange::Set(*level),
    };
    match guard
        .bound(transport, households, asked)
        .map_err(|f| f.detail)?
    {
        (
            Command::Volume {
                change: crate::request::VolumeChange::Set(level),
                ..
            },
            _,
        ) => Ok(RestoreOp::SetVolume {
            player: player.clone(),
            level,
        }),
        _ => Ok(op.clone()),
    }
}

fn no_dj_store() -> Failure {
    Failure::new(
        ErrorCode::Internal,
        "the DJ needs the daemon's store (its library, history and steering)",
    )
}

/// A control call's outcome as the action log keeps it: the decision
/// (`allow`, the policy's `clamp: ...`, or `deny: ...`), the result with any
/// healing noted, and the before-state undo restores. A failed call keeps its
/// before-state, since it may have half happened; `missed` zones the capture
/// could not read are noted, as undo cannot fully restore them.
fn log_entry(
    result: &Result<OutcomeDto, Failure>,
    before: Option<String>,
    missed: usize,
) -> (String, String, Option<String>) {
    let (decision, mut text, before) = match result {
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
            // A read-only no-op must not become an undo that can overwrite
            // later playback. Policy volume changes still keep their snapshot.
            let before = before.filter(|_| outcome.changed || !clamps.is_empty());
            let decision = if clamps.is_empty() {
                "allow".to_string()
            } else {
                format!("clamp: {}", clamps.join("; "))
            };
            let mut text = outcome.done.clone();
            for healed in noted(NoteCode::Healed) {
                let _ = write!(text, " (healed: {healed})");
            }
            (decision, text, before)
        }
        Err(f) if f.code == ErrorCode::PolicyDenied => {
            (format!("deny: {}", f.detail), f.detail.clone(), None)
        }
        Err(f) => ("allow".to_string(), format!("failed: {}", f.detail), before),
    };
    if missed > 0 {
        let _ = write!(
            text,
            " (before-state missing for {missed} zone(s): undo cannot fully restore)"
        );
    }
    (decision, text, before)
}

/// Whether `command` changes which rooms play together.
fn regroups(command: &Command) -> bool {
    matches!(
        command,
        Command::Join { .. } | Command::Leave { .. } | Command::Move { .. } | Command::Party { .. }
    )
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
        | Command::Dj { coordinator, .. }
        | Command::DjSteer {
            coordinator,
            start: true,
            ..
        } => vec![coordinator.clone()],
        Command::Volume { target, .. } | Command::Mute { target, .. } => vec![group_of(target)],
        Command::Join {
            member,
            coordinator,
        } => vec![group_of(member), coordinator.clone()],
        Command::Leave { member } => vec![group_of(member)],
        Command::Move { from, to, .. } => vec![group_of(from), group_of(to)],
        // Every group of the household joins.
        Command::Party { member, .. } => households
            .iter()
            .find(|h| h.player(member).is_some())
            .map_or_default(|h| h.groups.iter().map(|g| g.coordinator.clone()).collect()),
        // A steer changes the stored session, not the speakers.
        Command::DjSteer { start: false, .. } | Command::Nothing { .. } => Vec::new(),
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
    fn music_a_capped_caller_starts_plays_within_its_caps() {
        // Every room reads 80; the default cap is 70.
        let resume = |client: &Client| {
            let (s, sent) = surface("<CurrentVolume>80</CurrentVolume>", households());
            let out = s
                .control(client, "resume", |h| {
                    plan_transport(
                        h,
                        &ZoneRequest {
                            zone: "kitchen@s1".into(),
                        },
                        TransportAction::Resume,
                    )
                })
                .unwrap();
            let sent = sent.lock().unwrap().clone();
            (out, sent)
        };
        // An agent resumes Den's group: Kitchen and Den come down to 70 first.
        let (out, sent) = resume(&Client::McpStdio);
        let lowered = sent.iter().filter(|a| *a == "SetVolume").count();
        assert_eq!(lowered, 2, "{sent:?}");
        assert_eq!(sent.last().map(String::as_str), Some("Play"), "{sent:?}");
        let clamped: Vec<&Note> = out
            .notes
            .iter()
            .filter(|n| n.code == NoteCode::VolumeClamped)
            .collect();
        assert_eq!(clamped.len(), 2, "{:?}", out.notes);
        assert!(
            clamped[0].detail.contains("lowered from 80 to 70"),
            "{:?}",
            clamped[0]
        );
        // The CLI runs uncapped: nothing is read or lowered.
        let (out, sent) = resume(&Client::Cli);
        assert_eq!(sent, ["Play"]);
        assert!(out.notes.is_empty(), "{:?}", out.notes);
    }

    #[test]
    fn an_unidentified_caller_reuses_a_recent_doctor_report() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let runs = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&runs);
        let (s, _) = surface("", households());
        let s = s.with_doctor_checks(Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        let ran = || runs.load(Ordering::SeqCst);
        s.doctor(&Client::Unknown).unwrap();
        s.doctor(&Client::Unknown).unwrap();
        assert_eq!(ran(), 1, "the second call reused the first report");
        s.doctor(&Client::LoopbackHttp).unwrap();
        s.doctor(&Client::McpStdio).unwrap();
        assert_eq!(ran(), 3, "an identified caller always runs the checks");
        s.doctor(&Client::Unknown).unwrap();
        assert_eq!(ran(), 3, "and its report answers the next unknown one");
        if let Some((at, _)) = &mut *s.doctor_last.lock().unwrap() {
            *at = Instant::now().checked_sub(DOCTOR_REUSE).unwrap();
        }
        s.doctor(&Client::Unknown).unwrap();
        assert_eq!(ran(), 4, "an old report is made again");
    }

    /// Refuses the `refuse`d action with UPnP 701: every time, or only its
    /// `nth` (0-based) time.
    struct Refusing {
        refuse: &'static str,
        nth: Option<usize>,
        seen: Mutex<usize>,
    }

    impl Transport for Refusing {
        fn soap_post(
            &self,
            _: std::net::IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, fsonos_proto::ProtoError> {
            let action = action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default();
            if action == self.refuse {
                let mut seen = self.seen.lock().unwrap();
                let this = *seen;
                *seen += 1;
                if self.nth.is_none_or(|n| n == this) {
                    return Err(fsonos_proto::ProtoError::SoapFault {
                        code: 701,
                        reason: "Transition not available".into(),
                    });
                }
            }
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\"></u:{action}Response></s:Body></s:Envelope>"
            ))
        }
    }

    #[test]
    fn a_party_fails_only_when_no_room_could_join() {
        let party = |zone: &str| {
            let survey: Survey = Box::new(|_| Ok(households()));
            let s = Surface::new(
                Box::new(Refusing {
                    refuse: "SetAVTransportURI",
                    nth: None,
                    seen: Mutex::new(0),
                }),
                survey,
                Policy::default(),
                Box::new(fsonos_core::clock::SystemClock),
            );
            let req = crate::request::PartyRequest {
                zone: Some(zone.into()),
                household: None,
            };
            s.control(&Client::Cli, "group_all", |h| {
                crate::plan::plan_party(h, &req)
            })
        };
        // Patio leads S2 and its one other room refuses to join: no party.
        let err = party("Patio").unwrap_err();
        assert_eq!((err.code, err.upnp_code), (ErrorCode::UpnpFault, Some(701)));
        assert!(
            err.detail.starts_with("no room joined the party: Kitchen"),
            "{err:?}"
        );
        // Den leads S1, where Kitchen is in its group already: the studio that
        // refuses is noted, and the call answers.
        let out = party("Den").unwrap();
        assert_eq!(out.notes.len(), 1, "{:?}", out.notes);
        assert_eq!(out.notes[0].code, NoteCode::Skipped);
        assert!(
            out.notes[0].detail.contains("Studio skipped: "),
            "{:?}",
            out.notes
        );
    }

    #[test]
    fn a_set_steps_past_a_group_that_fails_and_notes_it() {
        let resume_everywhere = |nth: Option<usize>| {
            let survey: Survey = Box::new(|_| Ok(households()));
            let s = Surface::new(
                Box::new(Refusing {
                    refuse: "Play",
                    nth,
                    seen: Mutex::new(0),
                }),
                survey,
                Policy::default(),
                Box::new(fsonos_core::clock::SystemClock),
            );
            s.control_set(&Client::Cli, "resume", |h| {
                crate::plan::plan_transport_set(
                    h,
                    &ZoneRequest {
                        zone: "everywhere".into(),
                    },
                    TransportAction::Resume,
                )
            })
        };
        // One group of four has nothing to resume: the other three resume,
        // and the one is noted.
        let out = resume_everywhere(Some(1)).unwrap();
        assert_eq!(out.done.matches("resumed").count(), 3, "{}", out.done);
        let skipped: Vec<&Note> = out
            .notes
            .iter()
            .filter(|n| n.code == NoteCode::Skipped)
            .collect();
        assert_eq!(skipped.len(), 1, "{:?}", out.notes);
        assert!(skipped[0].detail.contains("skipped: "), "{:?}", skipped[0]);
        // None can: the call fails with the speaker's own fault.
        let err = resume_everywhere(None).unwrap_err();
        assert_eq!((err.code, err.upnp_code), (ErrorCode::UpnpFault, Some(701)));
    }

    #[test]
    fn undo_is_governed_by_the_undo_last_policy_key() {
        // Before the fix, undo authorized under "undo", so a deny of the tool's
        // real name (undo_last) did not stop it; now it does.
        let policy =
            Policy::from_toml("[clients.\"mcp-stdio\"]\ndeny = [\"undo_last\"]\n").unwrap();
        let survey: Survey = Box::new(|_| Ok(households()));
        let s = Surface::new(
            Box::new(super::testing::Canned {
                out_args: "",
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            survey,
            policy,
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_action_log(Box::new(fsonos_core::store::MemStore::default()), "test");
        let denied = s.undo(&Client::McpStdio, false).unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
    }

    #[test]
    fn an_edited_policy_file_takes_hold_without_a_restart() {
        let dir = std::env::temp_dir().join(format!("fsonos-policy-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(fsonos_core::policy::FILE_NAME);
        let write = |text: &str| std::fs::write(&file, text).unwrap();
        write("[defaults]\nmax_volume = 55\n");
        let survey: Survey = Box::new(|_| Ok(households()));
        let s = Surface::new(
            Box::new(super::testing::Canned {
                out_args: "",
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            survey,
            Policy::load(&dir).unwrap(),
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_action_log(Box::new(fsonos_core::store::MemStore::default()), "test")
        .with_policy_file(dir.clone());
        assert_eq!(s.policy().defaults.max_volume, 55);
        assert_eq!(s.undo(&Client::McpStdio, false).unwrap(), None);

        // The owner edits the file while the daemon runs: the next call obeys.
        write("[defaults]\nmax_volume = 40\n\n[clients.\"mcp-stdio\"]\ndeny = [\"undo_last\"]\n");
        assert_eq!(s.policy().defaults.max_volume, 40);
        let denied = s.undo(&Client::McpStdio, false).unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);

        // An edit that no longer parses leaves the last good policy in force.
        write("[defaults]\nmax_volume = \"loud\"\n");
        assert_eq!(s.policy().defaults.max_volume, 40);

        // Without the file, the defaults apply.
        std::fs::remove_file(&file).unwrap();
        assert_eq!(*s.policy(), Policy::default());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_edit_keeping_the_size_and_modification_time_still_takes_hold() {
        let dir = std::env::temp_dir().join(format!("fsonos-policy-same-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(fsonos_core::policy::FILE_NAME);
        std::fs::write(&file, "[defaults]\nmax_volume = 55\n").unwrap();
        let stamp = std::fs::metadata(&file).unwrap().modified().unwrap();
        let survey: Survey = Box::new(|_| Ok(households()));
        let s = Surface::new(
            Box::new(super::testing::Canned {
                out_args: "",
                sent: Arc::new(Mutex::new(Vec::new())),
            }),
            survey,
            Policy::load(&dir).unwrap(),
            Box::new(fsonos_core::clock::SystemClock),
        )
        .with_policy_file(dir.clone());
        assert_eq!(s.policy().defaults.max_volume, 55);

        // An edit of the same length, with the old modification time put
        // back (as a quick save within the same second can leave it).
        std::fs::write(&file, "[defaults]\nmax_volume = 40\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(stamp)
            .unwrap();
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), stamp);
        assert_eq!(s.policy().defaults.max_volume, 40);
        std::fs::remove_dir_all(&dir).unwrap();
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
    fn a_zones_state_says_whether_the_room_is_muted() {
        let (s, _) = surface(
            "<CurrentTransportState>PLAYING</CurrentTransportState>\
             <CurrentVolume>12</CurrentVolume><CurrentMute>1</CurrentMute>",
            households(),
        );
        let state = s.zone_state(&Client::Unknown, "Patio").unwrap();
        assert_eq!((state.volume, state.mute), (Some(12), Some(true)));
    }

    #[test]
    fn a_grouped_rooms_state_has_its_groups_volume() {
        let (s, sent) = surface(
            "<CurrentTransportState>PLAYING</CurrentTransportState>\
             <CurrentVolume>12</CurrentVolume>",
            households(),
        );
        // Kitchen@S1 plays in Den's group.
        let grouped = s.zone_state(&Client::Unknown, "Kitchen@S1").unwrap();
        assert_eq!(grouped.group_volume, Some(12), "{grouped:?}");
        assert!(sent.lock().unwrap().iter().any(|a| a == "GetGroupVolume"));
        let alone = s.zone_state(&Client::Unknown, "Patio").unwrap();
        assert_eq!(alone.group_volume, None, "{alone:?}");
    }

    #[test]
    fn a_zones_state_says_whether_it_shuffles_or_repeats() {
        let (s, sent) = surface(
            "<CurrentTransportState>PLAYING</CurrentTransportState>\
             <PlayMode>SHUFFLE_REPEAT_ONE</PlayMode>",
            households(),
        );
        let state = s.zone_state(&Client::Unknown, "Patio").unwrap();
        assert_eq!(
            (state.shuffle, state.repeat.as_deref()),
            (Some(true), Some("one"))
        );
        assert_eq!(state.play_mode.as_deref(), Some("SHUFFLE_REPEAT_ONE"));
        assert!(
            sent.lock()
                .unwrap()
                .iter()
                .any(|a| a == "GetTransportSettings")
        );
        assert_eq!(
            state.play_mode_text().as_deref(),
            Some("shuffle, repeat one")
        );
        // A player that doesn't say leaves both out.
        let (s, _) = surface(
            "<CurrentTransportState>PLAYING</CurrentTransportState>",
            households(),
        );
        let state = s.zone_state(&Client::Unknown, "Patio").unwrap();
        assert_eq!((state.shuffle, state.repeat.as_deref()), (None, None));
        assert_eq!(state.play_mode_text(), None);
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
